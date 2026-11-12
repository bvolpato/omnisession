use crate::codex::{
    CodexHistoryMode, DateTime, Digest, EventBuilder, EventKind, HashMap, MAX_CANONICAL_EVENTS,
    ReplayPolicy, Result, Sha256, Utc, Value, compact_tool_value, history_base, json,
    omitted_images_text, parse_timestamp, push_codex_session_metadata, saved_message, string_at,
    workspace_check,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(in crate::codex) fn native_legacy_header(payload: &Value) -> bool {
    CodexHistoryMode::from_payload(payload) == CodexHistoryMode::Legacy
        && (payload.get("history_mode").is_some()
            || ["originator", "cli_version"]
                .iter()
                .all(|key| payload[key].as_str().is_some_and(|value| !value.is_empty())))
}

pub(in crate::codex) fn history_event_type(payload: &Value) -> Option<&str> {
    let kind = payload["type"].as_str()?;
    let id = &payload["turn_id"];
    if matches!(
        kind,
        "turn_complete" | "task_complete" | "task_started" | "turn_started"
    ) && !id.is_null()
        && !id.is_string()
    {
        return None;
    }
    Some(kind)
}

pub(in crate::codex) fn client_submission(record: &Value) -> bool {
    record["type"] == "event_msg" && record["payload"]["type"] == "user_message"
}

pub(in crate::codex) fn turn_context_cwd(record: &Value) -> Option<&str> {
    record["payload"]["cwd"]
        .as_str()
        .filter(|cwd| !cwd.is_empty() && !cwd.contains('\0'))
}

pub(in crate::codex) fn push_record(
    builder: &mut EventBuilder,
    record: &Value,
    last_visible: &mut Option<MirrorGroup>,
    confirmed: bool,
) -> bool {
    let payload = &record["payload"];
    if record["type"] == "session_meta" {
        builder.codex_saved_users = native_legacy_header(payload).then_some(false);
        builder.codex_saved_assistant = false;
    } else if client_submission(record)
        && string_at(payload, &[&["message"], &["text"]]).is_some()
        && let Some(saved) = &mut builder.codex_saved_users
    {
        *saved = true;
    }
    if builder.codex_saved_users == Some(true)
        && record["type"] == "response_item"
        && payload["type"] == "message"
        && payload["role"] == "user"
    {
        MirrorGroup::barrier(last_visible);
        return false;
    }
    if confirmed {
        MirrorGroup::finish_provisional_batch(last_visible);
    }
    let position = builder.checkpoint();
    let user = project_record(builder, record, last_visible);
    if confirmed {
        MirrorGroup::finish_provisional_batch(last_visible);
        if let Some(group) = last_visible
            && user
            && position < builder.checkpoint()
        {
            group.confirmed_positions.insert(position);
            if group.confirmed_positions.len() > MAX_CANONICAL_EVENTS {
                group.confirmed_positions.pop_first();
            }
        }
    }
    user
}

fn project_record(
    builder: &mut EventBuilder,
    record: &Value,
    last_visible: &mut Option<MirrorGroup>,
) -> bool {
    let timestamp = parse_timestamp(record.get("timestamp"));
    let kind = record.get("type").and_then(Value::as_str);
    match kind {
        Some("compacted") => builder.push(
            EventKind::ProviderEvent,
            json!({"native_history_unverified":true,"reason":"The active Codex compaction checkpoint was not reconstructed."}),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.codex_compacted".to_owned()),
            None,
        ),
        Some("session_meta" | "turn_context") => {
            if let Some(payload) = record.get("payload") {
                push_codex_session_metadata(builder, payload, timestamp, false);
            }
        }
        Some("response_item") => {
            return push_response_item(builder, record, timestamp, last_visible);
        }
        Some("event_msg") => {
            if let Some(payload) = record.get("payload") {
                match history_event_type(payload) {
                    Some("task_started" | "turn_started") => {
                        *last_visible = payload["turn_id"].as_str().filter(|id| !id.is_empty()).map(|_| MirrorGroup::new(true));
                        builder.track_codex_mirror_openers(last_visible.is_some());
                    }
                    Some("turn_aborted") => MirrorGroup::barrier(last_visible),
                    Some("task_complete" | "turn_complete") if payload["turn_id"].is_null() => MirrorGroup::barrier(last_visible),
                    Some("task_complete" | "turn_complete" | "thread_rolled_back") => {
                        *last_visible = None;
                        builder.track_codex_mirror_openers(false);
                    }
                    _ => {}
                }
                push_codex_session_metadata(builder, payload, timestamp, true);
                return push_event_message(builder, record, timestamp, last_visible);
            }
        }
        _ => {}
    }
    false
}

#[derive(Clone)]
pub(in crate::codex) struct MirrorGroup {
    scoped: bool,
    next: usize,
    order: BTreeMap<usize, bool>,
    provisional_requests: VecDeque<(usize, u64)>,
    confirmed_positions: BTreeSet<u64>,
    saved_batch: bool,
    pub(super) batch_checkpoint: Option<u64>,
    last_text: Option<(MediaDigest, Option<MediaDigest>)>,
    pub(super) kind: EventKind,
    pending: HashMap<bool, PendingMessages>,
    image_pairs: ImagePairs,
    pub(super) paired_position: Option<usize>,
    native_position: Option<usize>,
    reorder: bool,
}

pub(super) enum MirrorObservation {
    Distinct,
    Paired(Option<usize>),
}

impl MirrorGroup {
    pub(in crate::codex) fn new(scoped: bool) -> Self {
        Self {
            scoped,
            next: 0,
            order: BTreeMap::new(),
            kind: EventKind::ProviderEvent,
            provisional_requests: VecDeque::new(),
            confirmed_positions: BTreeSet::new(),
            saved_batch: false,
            batch_checkpoint: None,
            last_text: None,
            pending: HashMap::new(),
            image_pairs: ImagePairs::default(),
            paired_position: None,
            native_position: None,
            reorder: false,
        }
    }

    pub(in crate::codex) fn barrier(slot: &mut Option<Self>) {
        if let Some(group) = slot {
            *group = Self::new(group.scoped);
        }
    }

    fn finish_provisional_batch(slot: &mut Option<Self>) {
        if let Some(group) = slot {
            group.provisional_requests.clear();
            group.saved_batch = false;
        }
    }

    fn publish_implicit_request(
        &mut self,
        builder: &mut EventBuilder,
        saved: bool,
        paired: bool,
    ) -> bool {
        if self.scoped || self.kind != EventKind::MessageUser {
            return false;
        }
        if !saved {
            if !paired {
                if self.provisional_requests.len() == MAX_CANONICAL_EVENTS {
                    self.provisional_requests.pop_front();
                }
                self.provisional_requests
                    .push_back((self.next - 1, builder.checkpoint()));
            }
            return false;
        }
        if paired
            && self
                .paired_position
                .and_then(|pos| u64::try_from(pos).ok())
                .is_some_and(|pos| self.confirmed_positions.remove(&pos))
        {
            return false;
        }
        if paired && self.provisional_requests.len() > 1 {
            self.batch_checkpoint = self
                .provisional_requests
                .front()
                .map(|&(_, position)| position);
            for (index, position) in self.provisional_requests.drain(..) {
                builder.remove_codex_mirror(position);
                if let Some((_, emitted)) = self
                    .pending
                    .get_mut(&false)
                    .and_then(|raw| raw.rows.get_mut(&index))
                {
                    *emitted = None;
                }
            }
            self.saved_batch = true;
        }
        let publish = self.saved_batch;
        self.saved_batch |= paired;
        self.provisional_requests.clear();
        publish
    }

    pub(super) fn observe(
        slot: &mut Option<Self>,
        message: VisibleMessage,
        position: Option<usize>,
    ) -> MirrorObservation {
        let group = slot.get_or_insert_with(|| Self::new(false));
        if group.kind != message.kind
            || (!group.scoped
                && group.provisional_requests.is_empty()
                && group.confirmed_positions.is_empty()
                && !group.saved_batch
                && !group
                    .last_text
                    .is_some_and(|(text, alternative)| message.matches_text(text, alternative)))
        {
            group.pending.clear();
            group.order.clear();
            group.image_pairs = ImagePairs::default();
            group.provisional_requests.clear();
            group.confirmed_positions.clear();
            group.saved_batch = false;
        }
        group.batch_checkpoint = None;
        group.kind.clone_from(&message.kind);
        group.last_text = Some((message.text, message.alternative));
        group.paired_position = None;
        group.reorder = false;
        let opposite = !message.event;
        group.image_pairs.add_credit();
        if let Some(candidates) = group.pending.get_mut(&opposite)
            && let Some((index, partner)) = group.image_pairs.match_row(candidates, &message)
        {
            let (candidate, prior) = &candidates.rows[&index];
            let prefer_text = candidate.text != message.text && message.event;
            let prior = *prior;
            group
                .image_pairs
                .remember(partner.as_ref().unwrap_or(&message), candidate);
            if message.kind == EventKind::MessageUser {
                candidates.remove(index);
                group.order.remove(&index);
                if candidates.rows.is_empty() {
                    group.pending.remove(&opposite);
                }
            }
            group.paired_position = prior;
            if message.event && message.kind == EventKind::MessageUser {
                group.reorder = prefer_text || (group.scoped && prior < group.native_position);
                group.native_position = if group.reorder { position } else { prior };
            }
            return MirrorObservation::Paired(prior);
        }
        if message.kind != EventKind::MessageUser {
            group.pending.clear();
            group.order.clear();
        }
        if message.event && message.kind == EventKind::MessageUser && group.scoped {
            group.native_position = position;
        }
        let key = message.event;
        group
            .pending
            .entry(key)
            .or_default()
            .insert(group.next, message, position);
        group.order.insert(group.next, key);
        group.next += 1;
        if group.order.len() > MAX_CANONICAL_EVENTS
            && let Some((index, key)) = group.order.pop_first()
            && let Some(candidates) = group.pending.get_mut(&key)
        {
            candidates.remove(index);
            if candidates.rows.is_empty() {
                group.pending.remove(&key);
            }
        }
        MirrorObservation::Distinct
    }
}

#[derive(Clone, Default)]
struct ImagePairs {
    by_text: HashMap<MediaDigest, Vec<[VisibleMessage; 2]>>,
    retained_parts: usize,
    credit: usize,
}

impl ImagePairs {
    fn add_credit(&mut self) {
        self.credit = (self.credit + 32).min(MAX_CANONICAL_EVENTS);
    }

    fn remember(&mut self, first: &VisibleMessage, other: &VisibleMessage) {
        let parts =
            first.media.as_ref().map_or(0, Vec::len) + other.media.as_ref().map_or(0, Vec::len);
        if self.retained_parts + parts > MAX_CANONICAL_EVENTS
            || ![first, other].iter().any(|row| {
                row.media.iter().flatten().any(|part| {
                    matches!(
                        part,
                        MessageMedia::Image { detail: None, .. }
                            | MessageMedia::Image {
                                path: Some(_),
                                url: Some(_),
                                ..
                            }
                            | MessageMedia::Image {
                                path: Some(_),
                                file: Some(_),
                                ..
                            }
                    )
                })
            })
        {
            return;
        }
        let pair = if first.event {
            [other.clone(), first.clone()]
        } else {
            [first.clone(), other.clone()]
        };
        self.by_text.entry(pair[1].text).or_default().push(pair);
        self.retained_parts += parts;
    }

    fn match_row(
        &mut self,
        pending: &PendingMessages,
        row: &VisibleMessage,
    ) -> Option<(usize, Option<VisibleMessage>)> {
        let mut direct_budget = usize::MAX;
        if let Some(index) = pending.matching(row, &mut direct_budget) {
            return Some((index, None));
        }
        for text in std::iter::once(row.text).chain(row.alternative) {
            if pending.indexes.contains_key(&MirrorKey::Text(text))
                && let Some(pairs) = self.by_text.get_mut(&text)
                && let Some(matched) = Self::rematch(pairs, text, pending, row, &mut self.credit)
            {
                return Some(matched);
            }
        }
        None
    }

    fn rematch(
        pairs: &mut [[VisibleMessage; 2]],
        text: MediaDigest,
        pending: &PendingMessages,
        row: &VisibleMessage,
        credit: &mut usize,
    ) -> Option<(usize, Option<VisibleMessage>)> {
        let side = usize::from(row.event);
        let mut visited = HashMap::<usize, Option<usize>>::new();
        let mut queue = VecDeque::<Option<usize>>::from([None]);
        while let Some(source) = queue.pop_front() {
            let current = source.map_or(row, |index| &pairs[index][side]);
            if let Some(mut index) = source
                && let Some(open) = pending.matching(current, credit)
                && pending.rows[&open].0.matches_text(text, None)
            {
                let displaced = current.clone();
                loop {
                    let previous = visited[&index];
                    pairs[index][side] = previous.map_or(row, |p| &pairs[p][side]).clone();
                    let Some(parent) = previous else { break };
                    index = parent;
                }
                return Some((open, Some(displaced)));
            }
            for (index, pair) in pairs.iter().enumerate() {
                *credit = credit.checked_sub(1 + current.media.as_ref().map_or(0, Vec::len))?;
                if !visited.contains_key(&index) && current.mirrors(&pair[1 - side]) {
                    visited.insert(index, source);
                    queue.push_back(Some(index));
                }
            }
        }
        None
    }
}

#[derive(Clone, Default)]
struct PendingMessages {
    rows: BTreeMap<usize, (VisibleMessage, Option<usize>)>,
    indexes: HashMap<MirrorKey, BTreeSet<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum MirrorKey {
    Text(MediaDigest),
    Client(Option<MediaDigest>),
    Item(Option<MediaDigest>),
    Media(usize, MediaDigest),
    Detail(usize, Option<MediaDigest>),
}

impl PendingMessages {
    fn insert(&mut self, index: usize, message: VisibleMessage, position: Option<usize>) {
        for keys in message.lookup_keys(false) {
            for key in keys {
                self.indexes.entry(key).or_default().insert(index);
            }
        }
        self.rows.insert(index, (message, position));
    }

    fn remove(&mut self, index: usize) {
        if let Some((message, _)) = self.rows.remove(&index) {
            for key in message.lookup_keys(false).into_iter().flatten() {
                if let Some(entries) = self.indexes.get_mut(&key) {
                    entries.remove(&index);
                    if entries.is_empty() {
                        self.indexes.remove(&key);
                    }
                }
            }
        }
    }

    fn matching(&self, message: &VisibleMessage, budget: &mut usize) -> Option<usize> {
        let groups = message.lookup_keys(true);
        let mut max = (0, 0, true);
        for key in groups
            .iter()
            .filter_map(|keys| keys.first())
            .filter(|key| self.indexes.contains_key(key))
        {
            match key {
                MirrorKey::Client(Some(_)) | MirrorKey::Item(Some(_)) => max.0 += 1,
                MirrorKey::Detail(_, Some(_)) => max.1 += 1,
                _ => {}
            }
        }
        let keys = groups.into_iter().min_by_key(|keys| {
            keys.iter()
                .filter_map(|key| self.indexes.get(key))
                .map(BTreeSet::len)
                .sum::<usize>()
        })?;
        let mut best = None;
        let mut consider = |index: &usize| {
            let Some(remaining) = budget.checked_sub(1) else {
                return true;
            };
            *budget = remaining;
            let candidate = &self.rows[index].0;
            if candidate.mirrors(message) {
                let score = candidate.specificity(message);
                if best.is_none_or(|(prev, pos)| score > prev || (score == prev && *index < pos)) {
                    best = Some((score, *index));
                }
                return score == max;
            }
            false
        };
        for rows in keys.iter().filter_map(|key| self.indexes.get(key)) {
            if rows.iter().any(&mut consider) {
                break;
            }
        }
        best.map(|(_, index)| index)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::codex) struct VisibleMessage {
    pub(super) kind: EventKind,
    text: MediaDigest,
    alternative: Option<MediaDigest>,
    event: bool,
    pub(super) item_id: Option<MediaDigest>,
    client_id: Option<MediaDigest>,
    media: Option<Vec<MessageMedia>>,
}

pub(in crate::codex) type MediaDigest = [u8; 32];
pub(in crate::codex) fn media_digest(value: &str) -> MediaDigest {
    Sha256::digest(value.as_bytes()).into()
}

#[derive(Clone, PartialEq, Eq)]
enum MessageMedia {
    Image {
        url: Option<MediaDigest>,
        file: Option<MediaDigest>,
        path: Option<MediaDigest>,
        detail: Option<MediaDigest>,
    },
    LocalAudio(MediaDigest),
    Audio {
        file: bool,
        reference: MediaDigest,
    },
}

impl MessageMedia {
    pub(in crate::codex) fn matches(&self, other: &Self) -> bool {
        if let (
            Self::Image {
                url,
                file,
                path,
                detail,
            },
            Self::Image {
                url: other_url,
                file: other_file,
                path: other_path,
                detail: other_detail,
            },
        ) = (self, other)
        {
            let refs = [
                url.as_ref().zip(other_url.as_ref()),
                file.as_ref().zip(other_file.as_ref()),
                path.as_ref().zip(other_path.as_ref()),
            ];
            return refs.iter().any(Option::is_some)
                && refs.into_iter().flatten().all(|(a, b)| a == b)
                && detail
                    .as_ref()
                    .zip(other_detail.as_ref())
                    .is_none_or(|(a, b)| a == b);
        }
        self == other
    }

    fn audio(value: &Value) -> Option<Self> {
        let (file, reference) = match (value["file_id"].as_str(), value["audio_url"].as_str()) {
            (Some(reference), None) => (true, reference),
            (None, Some(reference)) => (false, reference),
            _ => return None,
        };
        Some(Self::Audio {
            file,
            reference: media_digest(reference),
        })
    }
}

impl VisibleMessage {
    pub(in crate::codex) fn new(record: &Value, kind: EventKind, text: &str) -> Self {
        let event = record["type"] == "event_msg";
        let payload = &record["payload"];
        let item_id = [
            payload.get("item_id"),
            payload.get("message_id"),
            payload.get("id"),
            record.get("item_id"),
        ]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .map(media_digest);
        let media = if kind == EventKind::MessageUser {
            request_media(payload, event)
        } else {
            None
        };
        let alternative = if !event
            && kind == EventKind::MessageUser
            && payload["content"]
                .as_array()
                .is_some_and(|parts| parts.len() > 1)
        {
            let (parts, _) = message_parts(payload);
            (parts.len() > 1).then(|| {
                let mut hash = Sha256::new();
                for part in parts {
                    hash.update(part.as_bytes());
                }
                hash.finalize().into()
            })
        } else {
            None
        };
        Self {
            kind,
            text: media_digest(text),
            alternative,
            event,
            item_id,
            client_id: payload["client_id"]
                .as_str()
                .or_else(|| record["client_id"].as_str())
                .map(media_digest),
            media,
        }
    }

    fn lookup_keys(&self, matching: bool) -> Vec<Vec<MirrorKey>> {
        let mut groups = vec![
            std::iter::once(self.text)
                .chain(self.alternative)
                .map(MirrorKey::Text)
                .collect(),
        ];
        if self.kind != EventKind::MessageUser {
            return groups;
        }
        for (id, key) in [
            (&self.client_id, MirrorKey::Client as fn(_) -> _),
            (&self.item_id, MirrorKey::Item as fn(_) -> _),
        ] {
            let digest = *id;
            if !matching || digest.is_some() {
                let mut keys = vec![key(digest)];
                if matching {
                    keys.push(key(None));
                }
                groups.push(keys);
            }
        }
        for (index, media) in self.media.iter().flatten().enumerate() {
            if let MessageMedia::Image { detail, .. } = media {
                let digest = *detail;
                if !matching || digest.is_some() {
                    let mut keys = vec![MirrorKey::Detail(index, digest)];
                    if matching {
                        keys.push(MirrorKey::Detail(index, None));
                    }
                    groups.push(keys);
                }
            }
            let refs = match media {
                MessageMedia::Image {
                    url, file, path, ..
                } => [*url, *file, *path],
                MessageMedia::Audio { reference, .. } | MessageMedia::LocalAudio(reference) => {
                    [Some(*reference), None, None]
                }
            };
            groups.push(
                refs.into_iter()
                    .flatten()
                    .map(|digest| MirrorKey::Media(index, digest))
                    .collect(),
            );
        }
        groups
    }

    fn specificity(&self, other: &Self) -> (usize, usize, bool) {
        let ids = usize::from(self.item_id.is_some() && other.item_id.is_some())
            + usize::from(self.client_id.is_some() && other.client_id.is_some());
        let details = self
            .media
            .iter()
            .flatten()
            .zip(other.media.iter().flatten())
            .filter(|(left, right)| {
                matches!(
                    (left, right),
                    (
                        MessageMedia::Image {
                            detail: Some(_),
                            ..
                        },
                        MessageMedia::Image {
                            detail: Some(_),
                            ..
                        }
                    )
                )
            })
            .count();
        (ids, details, self.text == other.text)
    }

    fn matches_text(&self, text: MediaDigest, alternative: Option<MediaDigest>) -> bool {
        self.text == text || self.alternative == Some(text) || alternative == Some(self.text)
    }

    pub(super) fn mirrors(&self, other: &Self) -> bool {
        self.event != other.event
            && self.matches_text(other.text, other.alternative)
            && self.kind == other.kind
            && (self.kind != EventKind::MessageUser
                || (self
                    .item_id
                    .as_ref()
                    .zip(other.item_id.as_ref())
                    .is_none_or(|(a, b)| a == b)
                    && self
                        .client_id
                        .as_ref()
                        .zip(other.client_id.as_ref())
                        .is_none_or(|(a, b)| a == b)
                    && self.media.as_ref().zip(other.media.as_ref()).is_some_and(
                        |(left, right)| {
                            left.len() == right.len()
                                && left.iter().zip(right).all(|(a, b)| a.matches(b))
                        },
                    )))
    }
}

fn media_list(value: &Value) -> Option<&[Value]> {
    if value.is_null() {
        Some(&[])
    } else {
        value.as_array().map(Vec::as_slice)
    }
}

fn event_images(
    payload: &Value,
    field: &str,
    detail_field: &str,
) -> Option<VecDeque<MessageMedia>> {
    let values = media_list(&payload[field])?;
    let details = media_list(&payload[detail_field])?;
    if details.len() > values.len() {
        return None;
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let reference = media_digest(value.as_str()?);
            let detail = match details.get(index).filter(|v| !v.is_null()) {
                Some(v) => Some(media_digest(v.as_str()?)),
                None => None,
            };
            Some(MessageMedia::Image {
                url: (field == "images").then_some(reference),
                file: (field == "file_ids").then_some(reference),
                path: (field == "local_images").then_some(reference),
                detail,
            })
        })
        .collect()
}

fn ordered_media(
    mut inline: VecDeque<MessageMedia>,
    mut files: VecDeque<MessageMedia>,
    order: &Value,
) -> Option<Vec<MessageMedia>> {
    let order = media_list(order)?;
    if order.is_empty() {
        return Some(inline.into_iter().chain(files).collect());
    }
    if order.len() != inline.len() + files.len() {
        return None;
    }
    order
        .iter()
        .map(|kind| match kind.as_str()? {
            "inline" => inline.pop_front(),
            "file" => files.pop_front(),
            _ => None,
        })
        .collect()
}

fn event_media(payload: &Value) -> Option<Vec<MessageMedia>> {
    let mut media = ordered_media(
        event_images(payload, "images", "image_details")?,
        event_images(payload, "file_ids", "file_id_details")?,
        &payload["image_order"],
    )?;
    media.extend(event_images(
        payload,
        "local_images",
        "local_image_details",
    )?);
    let inline = media_list(&payload["audio"])?
        .iter()
        .map(|v| {
            Some(MessageMedia::Audio {
                file: false,
                reference: media_digest(v.as_str()?),
            })
        })
        .collect::<Option<VecDeque<_>>>()?;
    let refs = media_list(&payload["audio_references"])?
        .iter()
        .map(MessageMedia::audio)
        .collect::<Option<VecDeque<_>>>()?;
    if media_list(&payload["audio_order"])?.is_empty() && !refs.is_empty() {
        media.extend(refs);
    } else {
        media.extend(ordered_media(inline, refs, &payload["audio_order"])?);
    }
    for path in media_list(&payload["local_audio"])? {
        media.push(MessageMedia::LocalAudio(media_digest(path.as_str()?)));
    }
    Some(media)
}

fn request_media(payload: &Value, event: bool) -> Option<Vec<MessageMedia>> {
    if event {
        return event_media(payload);
    }
    let mut refs = Vec::new();
    let mut local_images = Vec::new();
    let mut audio = Vec::new();
    let mut local_audio = Vec::new();
    if let Some(parts) = payload["content"].as_array() {
        for (index, part) in parts.iter().enumerate() {
            match part["type"].as_str()? {
                "text" | "input_text" => {}
                "input_audio" => {
                    if let Some(path) = local_audio_path(parts, index) {
                        local_audio.push(MessageMedia::LocalAudio(media_digest(path)));
                    } else {
                        audio.push(MessageMedia::audio(part)?);
                    }
                }
                "input_image" => {
                    let path = local_image_path(parts, index).map(media_digest);
                    let file = part["file_id"].as_str().map(media_digest);
                    let url = part["image_url"].as_str().map(media_digest);
                    if file.is_some() == url.is_some() {
                        return None;
                    }
                    let detail = if part["detail"].is_null() {
                        None
                    } else {
                        Some(media_digest(part["detail"].as_str()?))
                    };
                    let image = MessageMedia::Image {
                        url,
                        file,
                        path,
                        detail,
                    };
                    if path.is_some() {
                        local_images.push(image);
                    } else {
                        refs.push(image);
                    }
                }
                _ => return None,
            }
        }
    }
    // User events preserve order within each media category.
    refs.extend(local_images);
    refs.extend(audio);
    refs.extend(local_audio);
    Some(refs)
}

fn push_message_text(
    builder: &mut EventBuilder,
    kind: EventKind,
    text: &str,
    timestamp: Option<DateTime<Utc>>,
    raw_type: &'static str,
    record: &Value,
    last_visible: &mut Option<MirrorGroup>,
) -> bool {
    if builder.codex_saved_users.is_some()
        && record["type"] == "event_msg"
        && kind == EventKind::MessageAssistant
        && !text.is_empty()
    {
        builder.codex_saved_assistant = true;
    }
    if (builder.codex_saved_users == Some(true) || builder.codex_saved_assistant)
        && record["type"] == "response_item"
        && kind == EventKind::MessageAssistant
        && text != omnis_core::CODEX_IMPORT_MARKER
    {
        return false;
    }
    let message = VisibleMessage::new(record, kind.clone(), text);
    let saved = message.event;
    let request = kind == EventKind::MessageUser && !is_contextual_user_text(text);
    if !saved
        && request
        && !text.is_empty()
        && !last_visible.as_ref().is_some_and(|group| group.scoped)
    {
        builder.track_codex_provisional_opener();
    }
    if (!request || text.is_empty())
        && let Some(group) = last_visible
    {
        group.provisional_requests.clear();
        group.saved_batch = false;
    }
    let suppressed =
        !saved && request && last_visible.as_ref().is_some_and(|group| group.saved_batch);
    let emitted = if suppressed || text.is_empty() {
        None
    } else {
        usize::try_from(builder.checkpoint()).ok()
    };
    let observation = MirrorGroup::observe(last_visible, message, emitted);
    let paired = matches!(observation, MirrorObservation::Paired(_));
    if let MirrorObservation::Paired(position) = observation
        && kind == EventKind::MessageAssistant
        && builder.codex_saved_users.is_some()
        && let Some(group) = last_visible
    {
        *group = MirrorGroup::new(group.scoped);
        group.paired_position = position;
        return false;
    }
    if suppressed && !paired {
        return false;
    }
    let missing_raw = saved && matches!(observation, MirrorObservation::Paired(None));
    let publish_saved = request
        && !text.is_empty()
        && last_visible.as_mut().is_some_and(|group| {
            group.publish_implicit_request(builder, saved, paired) || missing_raw
        });
    if !publish_saved && let MirrorObservation::Paired(position) = observation {
        if !last_visible.as_ref().is_some_and(|group| group.reorder) || text.is_empty() {
            return false;
        }
        if let Some(position) = position.and_then(|pos| u64::try_from(pos).ok()) {
            if !last_visible.as_ref().is_some_and(|group| group.scoped) {
                builder.replace_codex_user_text(position, text, timestamp);
                return false;
            }
            builder.remove_codex_mirror(position);
        }
    }
    let user_turn = (!paired || publish_saved)
        && kind == EventKind::MessageUser
        && !is_contextual_user_text(text);
    if text.is_empty() {
        return user_turn;
    }
    builder.push(
        kind,
        json!({ "text": text }),
        timestamp,
        ReplayPolicy::Contextual,
        Some(raw_type.to_owned()),
        None,
    );
    user_turn
}

pub(in crate::codex) fn is_contextual_user_text(text: &str) -> bool {
    let text = text.trim_start();
    [
        "# AGENTS.md instructions for ",
        "<environment_context>",
        "<turn_aborted>",
        "<user_shell_command>",
        "<subagent_notification>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub(in crate::codex) fn local_image_path(parts: &[Value], index: usize) -> Option<&str> {
    let opener = parts.get(index.checked_sub(1)?)?;
    if opener["type"] != "input_text" {
        return None;
    }
    let text = opener["text"].as_str()?;
    if !is_image_wrapper(text, false, true) {
        return None;
    }
    let (_, path) = text.strip_suffix("\">")?.rsplit_once(" path=\"")?;
    (!path.is_empty()).then_some(path)
}

pub(in crate::codex) fn local_audio_path(parts: &[Value], index: usize) -> Option<&str> {
    let text = |index| {
        let part: &Value = parts.get(index)?;
        if part["type"] != "input_text" {
            return None;
        }
        part["text"].as_str()
    };
    let audio = parts.get(index)?;
    if audio["type"] != "input_audio"
        || audio["audio_url"].as_str()?.is_empty()
        || !audio["file_id"].is_null()
        || text(index + 1)? != "</audio>"
    {
        return None;
    }
    let open = text(index.checked_sub(1)?)?.strip_prefix("<audio name=[Audio #")?;
    let (number, path) = open.strip_suffix("\">")?.split_once("] path=\"")?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && !path.is_empty())
        .then_some(path)
}

pub(in crate::codex) fn omitted_media_text(payload: &Value, images: usize) -> Option<String> {
    if images > 0 {
        return Some(omitted_images_text(images));
    }
    let has_audio = payload["content"]
        .as_array()
        .is_some_and(|parts| parts.iter().any(|p| p["type"] == "input_audio"))
        || ["audio", "audio_references", "local_audio"]
            .iter()
            .any(|field| {
                payload[*field]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            });
    has_audio.then(|| "[audio omitted]".to_owned())
}

pub(in crate::codex) fn event_scope_matches(record: &Value, owner: &str) -> bool {
    [record, &record["payload"]].iter().all(|object| {
        object.get("thread_id").is_none_or(|id| {
            id.as_str()
                .is_some_and(|id| workspace_check::same_thread(owner, id))
        })
    }) && record
        .get("turn_id")
        .is_none_or(|id| id == &record["payload"]["turn_id"])
}

pub(in crate::codex) fn receipt_in_turn(
    record: &Value,
    owner: &str,
    active_turn: Option<&str>,
) -> bool {
    let payload = &record["payload"];
    record["type"] == "event_msg"
        && payload["type"] == "item_completed"
        && payload["thread_id"].is_string()
        && event_scope_matches(record, owner)
        && payload["turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && active_turn.is_none_or(|active| active == id))
}

pub(in crate::codex) struct RequestReceipts<'a, M> {
    owner: &'a str,
    contiguous: bool,
    turn: Option<String>,
    pending: Option<(Value, M)>,
}

impl<'a, M: Copy + Eq> RequestReceipts<'a, M> {
    pub(in crate::codex) fn new(owner: &'a str, contiguous: bool) -> Self {
        Self {
            owner,
            contiguous,
            turn: None,
            pending: None,
        }
    }

    pub(in crate::codex) fn observe_confirmed(
        &mut self,
        record: Value,
        mark: M,
        mut visit: impl FnMut(Value, M, bool) -> Result<()>,
    ) -> Result<()> {
        let record = saved_message::project(record);
        let payload = &record["payload"];
        if let Some((mut request, before)) = self.pending.take() {
            let mut confirmed = false;
            if before == mark
                && (self.contiguous || request["ordinal"].is_u64())
                && receipt_in_turn(&record, self.owner, self.turn.as_deref())
                && let Some(parts) = history_base::receipt_literal_parts(&request, &record)
            {
                history_base::project_literal_parts(&mut request, &parts);
                confirmed = true;
            }
            visit(request, before, confirmed)?;
        }
        if record["type"] == "event_msg" && event_scope_matches(&record, self.owner) {
            match history_event_type(payload) {
                Some("task_started" | "turn_started") => {
                    self.turn = payload["turn_id"].as_str().map(str::to_owned);
                }
                Some("task_complete" | "turn_complete" | "turn_aborted" | "thread_rolled_back") => {
                    self.turn = None;
                }
                _ => {}
            }
        }
        if record["type"] == "response_item"
            && payload["role"] == "user"
            && payload["id"].is_string()
        {
            self.pending = Some((record, mark));
            Ok(())
        } else {
            visit(record, mark, false)
        }
    }

    pub(in crate::codex) fn flush(
        &mut self,
        mut visit: impl FnMut(Value, M) -> Result<()>,
    ) -> Result<()> {
        if let Some((record, mark)) = self.pending.take() {
            visit(record, mark)?;
        }
        Ok(())
    }
}

/// Visible text parts of a message payload and how many images it carries.
///
/// Codex adds framing around local images and audio. Only the adjacent user text is visible.
pub(in crate::codex) fn message_parts(payload: &Value) -> (Vec<&str>, usize) {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let part_type = |index: Option<usize>| {
        index
            .and_then(|index| content.get(index))
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
    };
    let images = (0..content.len())
        .filter(|index| part_type(Some(*index)) == Some("input_image"))
        .count();
    let parts = content
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            matches!(
                part_type(Some(*index)),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|(index, part)| Some((index, part.get("text").and_then(Value::as_str)?)))
        .filter(|(index, text)| {
            let audio_wrapper = (text.starts_with("<audio name=")
                && local_audio_path(content, index + 1).is_some())
                || (*text == "</audio>"
                    && index
                        .checked_sub(1)
                        .is_some_and(|i| local_audio_path(content, i).is_some()));
            !audio_wrapper
                && (part_type(Some(*index)) == Some("text")
                    || !is_image_wrapper(
                        text,
                        part_type(index.checked_sub(1)) == Some("input_image"),
                        part_type(Some(index + 1)) == Some("input_image"),
                    ))
        })
        .map(|(_, text)| text)
        .collect();
    (parts, images)
}

fn push_response_item(
    builder: &mut EventBuilder,
    record: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<MirrorGroup>,
) -> bool {
    let payload = &record["payload"];
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if item_type == "message" {
        let kind = match payload.get("role").and_then(Value::as_str) {
            Some("user") => Some(EventKind::MessageUser),
            Some("assistant") => Some(EventKind::MessageAssistant),
            _ => None,
        };
        let Some(kind) = kind else {
            return false;
        };
        if let Some(text) = payload.get("content").and_then(Value::as_str) {
            return push_message_text(
                builder,
                kind,
                text,
                timestamp,
                "response_item.message",
                record,
                last_visible,
            );
        }
        let (parts, images) = message_parts(payload);
        let contextual = parts.iter().any(|text| is_contextual_user_text(text));
        let text = if parts.is_empty() && kind == EventKind::MessageUser {
            omitted_media_text(payload, images).unwrap_or_default()
        } else {
            parts.join("\n")
        };
        return push_message_text(
            builder,
            kind,
            &text,
            timestamp,
            "response_item.message",
            record,
            last_visible,
        ) && !contextual;
    }

    let kind = match item_type {
        "function_call" | "custom_tool_call" | "web_search_call" | "computer_call" => {
            Some(EventKind::ToolCalled)
        }
        "local_shell_call" => Some(EventKind::CommandExecuted),
        "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "computer_call_output" => {
            let failed = payload
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "failed" | "error"));
            Some(if failed {
                EventKind::ToolFailed
            } else {
                EventKind::ToolCompleted
            })
        }
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    builder.push(
        kind,
        compact_tool_value(payload, None),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(format!("response_item.{item_type}")),
        None,
    );
    MirrorGroup::barrier(last_visible);
    false
}

fn push_event_message(
    builder: &mut EventBuilder,
    record: &Value,
    timestamp: Option<DateTime<Utc>>,
    last_visible: &mut Option<MirrorGroup>,
) -> bool {
    let payload = &record["payload"];
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let kind = match event_type {
        "user_message" => Some(EventKind::MessageUser),
        "agent_message" => Some(EventKind::MessageAssistant),
        _ => None,
    };
    let Some(kind) = kind else {
        return false;
    };
    let text = string_at(payload, &[&["message"], &["text"]]).unwrap_or("");
    // The import notification has no response item. A literal reply with the same
    // text has one, so let the response item supply its role and content.
    if kind == EventKind::MessageAssistant && text == omnis_core::CODEX_IMPORT_MARKER {
        return false;
    }
    let images = ["images", "file_ids", "local_images"]
        .into_iter()
        .filter_map(|field| payload.get(field).and_then(Value::as_array))
        .map(Vec::len)
        .sum::<usize>();
    if text.is_empty()
        && kind == EventKind::MessageUser
        && let Some(placeholder) = omitted_media_text(payload, images)
    {
        return push_message_text(
            builder,
            kind,
            &placeholder,
            timestamp,
            "event_msg",
            record,
            last_visible,
        );
    }
    push_message_text(
        builder,
        kind,
        text,
        timestamp,
        "event_msg",
        record,
        last_visible,
    )
}

pub(super) fn is_image_wrapper(text: &str, image_before: bool, image_after: bool) -> bool {
    let opens = text == "<image>"
        || text
            .strip_prefix("<image name=")
            .is_some_and(|rest| rest.ends_with('>'));
    (opens && image_after) || (text == "</image>" && image_before)
}
