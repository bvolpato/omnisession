//! Relate native model rollbacks to completed display inputs in paginated Codex history.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{MAX_CANONICAL_EVENTS, failed_attachments, message_parts};

mod migration_scopes;

type Key = [u8; 32];
type IdIndex<T> = HashMap<(u64, Option<Key>, Key), Vec<(T, Input)>>;

fn key(text: &str) -> Key {
    Sha256::digest(text.as_bytes()).into()
}

fn uuid_v7(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::try_parse(value)
            .is_ok_and(|id| id.get_variant() == uuid::Variant::RFC4122 && id.get_version_num() == 7)
}

/// Codex classifies the original response item before it renders visible content.
pub(super) fn model_instruction(payload: &Value) -> bool {
    let Some(content) = payload["content"].as_array() else {
        return false;
    };
    match (payload["type"].as_str(), payload["role"].as_str()) {
        (Some("message"), Some("user")) => !content.iter().any(|part| {
            part["type"] == "input_text" && part["text"].as_str().is_some_and(contextual_fragment)
        }),
        _ => is_private_instruction(payload),
    }
}

pub(super) fn is_private_instruction(payload: &Value) -> bool {
    let Some(content) = payload["content"].as_array() else {
        return false;
    };
    if payload["type"] == "agent_message" {
        return payload["author"].is_string()
            && payload["recipient"].is_string()
            && content.iter().all(|part| match part["type"].as_str() {
                Some("input_text") => part["text"].is_string(),
                Some("encrypted_content") => part["encrypted_content"].is_string(),
                _ => false,
            });
    }
    let [part] = content.as_slice() else {
        return false;
    };
    if payload["type"] != "message"
        || payload["role"] != "assistant"
        || !matches!(part["type"].as_str(), Some("input_text" | "output_text"))
    {
        return false;
    }
    let Some(text) = part["text"]
        .as_str()
        .filter(|text| text.trim_start().starts_with('{'))
    else {
        return false;
    };
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    communication(&message)
}

pub(super) fn communication(message: &Value) -> bool {
    let path = |value: &Value| value.as_str().is_some_and(agent_path);
    path(&message["author"])
        && path(&message["recipient"])
        && message["content"].is_string()
        && message["trigger_turn"].is_boolean()
        && message
            .get("other_recipients")
            .is_none_or(|field| field.as_array().is_some_and(|group| group.iter().all(path)))
        && ["id", "encrypted_content"]
            .iter()
            .all(|field| message[*field].is_null() || message[*field].is_string())
        && {
            let metadata = &message["internal_chat_message_metadata_passthrough"];
            metadata.is_null()
                || (metadata.is_object()
                    && (metadata["turn_id"].is_null() || metadata["turn_id"].is_string())
                    && (metadata["create_time"].is_null() || metadata["create_time"].is_number()))
        }
}

fn agent_path(path: &str) -> bool {
    path == "/morpheus"
        || path == "/root"
        || path.strip_prefix("/root/").is_some_and(|rest| {
            rest.split('/').all(|name| {
                !name.is_empty()
                    && name != "root"
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
        })
}

const SUMMARY_PREFIX: &str = concat!(
    "Another language model started to solve this problem and produced a summary of its thinking process. ",
    "You also have access to the state of the tools that were used by that language model. ",
    "Use this to build on the work that has already been done and avoid duplicating work. ",
    "Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:\n",
);

fn contextual_fragment(text: &str) -> bool {
    if text.starts_with(SUMMARY_PREFIX) {
        return true;
    }
    let text = text.trim();
    if text.starts_with("Warning: ") {
        return text.starts_with(
            "Warning: The maximum number of unified exec processes you can keep open is",
        ) || text.starts_with(
            "Warning: Your account was flagged for potentially high-risk cyber activity",
        ) || (text.starts_with("Warning: apply_patch was requested via ")
            && text.ends_with("Use the apply_patch tool instead of exec_command."));
    }
    if !text.starts_with(['<', '#']) {
        return false;
    }
    let marked = |open: &str, close: &str| {
        text.get(..open.len())
            .is_some_and(|part| part.eq_ignore_ascii_case(open))
            && text
                .get(text.len().saturating_sub(close.len())..)
                .is_some_and(|part| part.eq_ignore_ascii_case(close))
    };
    [
        ("# AGENTS.md instructions", "</INSTRUCTIONS>"),
        ("<environment_context>", "</environment_context>"),
        (
            "<agent_message_board_notification>",
            "</agent_message_board_notification>",
        ),
        ("<skill>", "</skill>"),
        ("<user_shell_command>", "</user_shell_command>"),
        ("<turn_aborted>", "</turn_aborted>"),
        ("<subagent_notification>", "</subagent_notification>"),
        ("<recommended_plugins>", "</recommended_plugins>"),
    ]
    .iter()
    .any(|(open, close)| marked(open, close))
        || (text.starts_with("<goal_context>") && text.ends_with("</goal_context>"))
        || text
            .strip_prefix("<external_")
            .and_then(|rest| rest.split_once('>'))
            .is_some_and(|(name, tail)| {
                tail.strip_suffix('>')
                    .and_then(|tail| tail.strip_suffix(name))
                    .is_some_and(|prefix| prefix.ends_with("</external_"))
            })
        || text
            .strip_prefix("<codex_internal_context source=\"")
            .and_then(|rest| rest.split_once("\">"))
            .is_some_and(|(source, tail)| {
                source
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                    && source
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    && tail.ends_with("</codex_internal_context>")
            })
        || hook_fragment(text)
}

fn hook_fragment(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(rename = "hook_prompt")]
    struct HookPrompt {
        #[serde(rename = "@hook_run_id")]
        run_id: String,
        #[serde(rename = "$text")]
        _text: String,
    }
    text.contains("hook_run_id")
        && quick_xml::de::from_str::<HookPrompt>(text)
            .is_ok_and(|hook| !hook.run_id.trim().is_empty())
}

fn input_spelling(payload: &Value, display: bool) -> (Option<Key>, Option<Key>) {
    let Some(parts) = payload["content"].as_array() else {
        return (None, None);
    };
    let tag = if display { "text" } else { "input_text" };
    let attachment = if display {
        parts
            .iter()
            .any(|part| {
                matches!(
                    part["type"].as_str(),
                    Some("local_image" | "local_audio" | "image" | "audio")
                )
            })
            .then(|| {
                let text: Vec<_> = parts
                    .iter()
                    .filter(|p| p["type"] == tag)
                    .filter_map(|p| p["text"].as_str())
                    .collect();
                failed_attachments::completed(parts, &text)
            })
            .flatten()
    } else {
        failed_attachments::model(parts, &message_parts(parts, |_, _| true))
    };
    let mut hash = Sha256::new();
    // Plain text and attachment paths must have distinct signatures.
    hash.update([0_u8]);
    let mut nonempty = false;
    for part in parts {
        let Some(text) = part["text"].as_str().filter(|_| part["type"] == tag) else {
            return (None, attachment);
        };
        nonempty |= !text.is_empty();
        hash.update(text.as_bytes());
    }
    (nonempty.then(|| hash.finalize().into()), attachment)
}

#[derive(Clone, Copy)]
struct MediaShape {
    signature: Key,
    local: usize,
}

fn media_shape(payload: &Value, display: bool) -> Option<MediaShape> {
    let parts = payload["content"].as_array()?;
    let (mut images, mut audio, mut local) = (0_usize, 0_usize, 0_usize);
    for (i, part) in parts.iter().enumerate() {
        match part["type"].as_str() {
            Some("local_image" | "image" | "input_image") => images += 1,
            Some("local_audio" | "audio" | "input_audio") => audio += 1,
            _ => continue,
        }
        if matches!(part["type"].as_str(), Some("local_image" | "local_audio"))
            || (!display && failed_attachments::local_path(parts, i).is_some())
        {
            local += 1;
        }
    }
    if images + audio == 0 {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(images.to_be_bytes());
    hash.update(audio.to_be_bytes());
    for part in message_parts(parts, |_, _| true) {
        hash.update(part.as_bytes());
    }
    Some(MediaShape {
        signature: hash.finalize().into(),
        local,
    })
}

fn content_digest(payload: &Value) -> Key {
    json_digest(&payload["content"])
}

// A checkpoint can reuse an ID after editing a message, an attachment, or a tool result.
pub(super) fn json_digest(value: &impl serde::Serialize) -> Key {
    struct Writer(Sha256);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Sha256::new());
    serde_json::to_writer(&mut writer, value).expect("serializing JSON to a digest");
    writer.0.finalize().into()
}

pub(super) struct ConfirmedInputCopies {
    by_id: HashMap<(Key, Key), (u64, Key)>,
    original: HashSet<u64>,
}

impl ConfirmedInputCopies {
    pub(super) fn original_display_users(&self) -> &HashSet<u64> {
        &self.original
    }

    fn witness(&self, item: &Value) -> Option<&(u64, Key)> {
        if item["type"] != "message" || item["role"] != "user" {
            return None;
        }
        let turn = item["internal_chat_message_metadata_passthrough"]["turn_id"].as_str()?;
        self.by_id.get(&(key(turn), key(item["id"].as_str()?)))
    }

    pub(super) fn has_witness(&self, item: &Value) -> bool {
        self.witness(item).is_some()
    }

    pub(super) fn get(&self, item: &Value) -> Option<u64> {
        let (sequence, content) = self.witness(item)?;
        (*content == content_digest(item)).then_some(*sequence)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdKind {
    InterAgent,
    Stable,
    ModelInput,
    GeneratedDisplay,
    LiveDisplay,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Text,
    Attachment,
    Ambiguous,
}

#[derive(Clone, Copy)]
struct Input {
    segment: u64,
    window: u64,
    withdrawal: Option<Withdrawal>,
    turn: Option<Key>,
    generated_turn: bool,
    id: Option<Key>,
    kind: IdKind,
    text: Option<Key>,
    spelling: Spelling,
    literal: Option<Key>,
    model_content: Option<Key>,
    media: Option<MediaShape>,
    ordinal: u64,
    // A migrated request can have a generated turn opening immediately before its display.
    display_start: u64,
    order: Option<u64>,
    accepted_output: Option<u64>,
    inherited: bool,
}

impl Input {
    fn new(turn: Option<&str>, payload: &Value, display: bool, ordinal: u64, segment: u64) -> Self {
        let (plain, attachment) = input_spelling(payload, display);
        let id = payload["id"].as_str().filter(|id| !id.is_empty());
        let kind = match id {
            Some(id) if display && uuid_v7(id) => IdKind::LiveDisplay,
            Some(id)
                if display
                    && id.strip_prefix("item-").is_some_and(|n| {
                        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                    }) =>
            {
                IdKind::GeneratedDisplay
            }
            Some(id)
                if !display
                    && (id.strip_prefix("msg_").is_some_and(uuid_v7)
                        || id.strip_prefix("msg_input_").is_some_and(|s| {
                            s.len() == 36 && uuid::Uuid::try_parse(s).is_ok()
                        })) =>
            {
                IdKind::ModelInput
            }
            _ => IdKind::Stable,
        };
        Self {
            segment,
            window: 0,
            withdrawal: None,
            turn: turn.filter(|v| !v.is_empty()).map(key),
            generated_turn: turn.is_some_and(super::is_implicit_turn),
            id: id.map(key),
            kind,
            text: attachment.or(plain),
            spelling: if attachment.is_some() {
                Spelling::Attachment
            } else {
                Spelling::Text
            },
            literal: if display || attachment.is_none() {
                None
            } else {
                plain
            },
            media: media_shape(payload, display),
            model_content: (!display && turn.is_some() && id.is_some())
                .then(|| content_digest(payload)),
            ordinal,
            display_start: ordinal,
            order: None,
            accepted_output: None,
            inherited: false,
        }
    }

    fn permits_text(self, shown: Self) -> bool {
        self.id
            .zip(shown.id)
            .is_none_or(|(raw, display)| raw == display)
            || shown.kind == IdKind::GeneratedDisplay
            || (self.kind == IdKind::ModelInput && shown.kind == IdKind::LiveDisplay)
    }

    fn adjacent(self, shown: Self) -> bool {
        self.ordinal.abs_diff(shown.ordinal) == 1
            || self.ordinal.checked_add(1) == Some(shown.display_start)
    }
}

#[derive(Clone, Copy)]
pub(super) struct InputPosition {
    ordinal: u64,
    order: Option<u64>,
}

impl InputPosition {
    pub(super) fn user(record: &Value, ordinal: u64) -> Option<Self> {
        let payload = &record["payload"];
        (record["type"] == "response_item"
            && payload["role"] == "user"
            && model_instruction(payload))
        .then(|| Self {
            ordinal,
            order: super::model_cut::input_order(&record["metadata"]),
        })
    }
}

// The legacy public turn list excludes inter-agent inputs that count toward a model rollback.
#[derive(Default)]
pub(super) struct LegacyPrivateInputs {
    entries: VecDeque<InputPosition>,
    evicted_through: u64,
}

impl LegacyPrivateInputs {
    pub(super) fn observe(&mut self, record: &Value, ordinal: u64) -> bool {
        let payload = &record["payload"];
        let private = match record["type"].as_str() {
            Some("inter_agent_communication") => communication(payload),
            Some("response_item") => is_private_instruction(payload),
            _ => false,
        };
        if private {
            if self.entries.len() == MAX_CANONICAL_EVENTS
                && let Some(old) = self.entries.pop_front()
            {
                self.evicted_through = old.ordinal;
            }
            self.entries.push_back(InputPosition {
                ordinal,
                order: super::model_cut::acceptance_order(payload, &record["metadata"]),
            });
        }
        private
    }

    pub(super) fn crosses(&mut self, cut: InputPosition, excess: u64) -> bool {
        let keep = self
            .entries
            .partition_point(|entry| entry.ordinal < cut.ordinal);
        let hidden = (self.entries.len() - keep) as u64;
        let evicted = self
            .evicted_through
            .checked_sub(cut.ordinal)
            .map_or(0, |span| span.saturating_add(1));
        self.entries.truncate(keep);
        if cut.order.is_some() {
            self.entries
                .retain(|entry| !super::model_cut::removes_accepted(entry.order, cut.order));
        }
        hidden.saturating_add(evicted) > excess
    }
}

#[derive(Default)]
pub(super) struct PaginatedRollback {
    model: VecDeque<Input>,
    displayed: BTreeMap<u64, Input>,
    generated_start: Option<(Key, u64)>,
    segment: u64,
    window: u64,
    evicted: usize,
    // Text on opposite sides of a retention boundary cannot prove a shared input.
    history_floor: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Withdrawal {
    OutsideCheckpoint,
    Confirmed,
    Unverified,
}

impl Withdrawal {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::OutsideCheckpoint | Self::Unverified => "unverified",
            Self::Confirmed => "withdrawn",
        }
    }
}

impl PaginatedRollback {
    pub(super) fn has_inputs(&self) -> bool {
        !self.model.is_empty() || self.evicted > 0
    }

    pub(super) fn segment(&mut self) {
        self.segment = self.segment.saturating_add(1);
        self.window = self.window.saturating_add(1);
        self.generated_start = None;
    }

    pub(super) fn turn_started(&mut self, turn: Option<&str>, ordinal: u64) {
        self.generated_start = turn
            .filter(|turn| super::is_implicit_turn(turn))
            .map(|turn| (key(turn), ordinal));
    }

    pub(super) fn model_user(&mut self, record: &Value, ordinal: u64) -> bool {
        let payload = &record["payload"];
        let standalone = record["type"] == "inter_agent_communication";
        let admitted = if standalone {
            communication(payload)
        } else {
            model_instruction(payload)
        };
        if !admitted {
            return false;
        }
        let turn = payload["internal_chat_message_metadata_passthrough"]["turn_id"].as_str();
        let user = !standalone && payload["type"] == "message" && payload["role"] == "user";
        let spelling = if user { payload } else { &Value::Null };
        let mut input = Input::new(turn, spelling, false, ordinal, self.segment);
        if !user {
            input.kind = IdKind::InterAgent;
            input.accepted_output =
                super::model_cut::acceptance_order(payload, &record["metadata"]);
        }
        input.window = self.window;
        input.inherited = !standalone && record["metadata"]["inherited_user_message"] == true;
        if !standalone {
            input.order = super::model_cut::input_order(&record["metadata"]);
        }
        if self.model.len() == MAX_CANONICAL_EVENTS
            && let Some(old) = self.model.pop_front()
        {
            self.evicted = self.evicted.saturating_add(1);
            self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
        }
        self.model.push_back(input);
        true
    }

    pub(super) fn display_user(&mut self, sequence: u64, turn: &str, item: &Value, ordinal: u64) {
        let mut input = Input::new(Some(turn), item, true, ordinal, self.segment);
        input.window = self.window;
        if let Some((scope, opened)) = self.generated_start
            && Some(scope) == input.turn
            && opened.checked_add(1) == Some(ordinal)
        {
            input.display_start = opened;
        }
        self.displayed
            .entry(sequence)
            .and_modify(|old| {
                // A revised display stays at its original position. Changed text loses its old
                // content-based identity until the model and display IDs identify the same item.
                if old.text != input.text {
                    old.text = None;
                    old.media = None;
                }
            })
            .or_insert(input);
        if self.displayed.len() > MAX_CANONICAL_EVENTS
            && let Some((_, old)) = self.displayed.pop_first()
        {
            self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
        }
    }

    pub(super) fn discard_before(&mut self, sequence: u64) {
        while self
            .displayed
            .first_key_value()
            .is_some_and(|(seq, _)| *seq < sequence)
        {
            if let Some((_, old)) = self.displayed.pop_first() {
                self.history_floor = self.history_floor.max(old.ordinal.saturating_add(1));
            }
        }
    }

    pub(super) fn display_is_plain_text(&self, sequence: u64) -> bool {
        self.displayed.get(&sequence).is_some_and(|shown| {
            shown.spelling == Spelling::Text && shown.text.is_some() && shown.media.is_none()
        })
    }

    pub(super) fn count_after(&self, ordinal: u64) -> usize {
        self.model.len() - self.model.partition_point(|input| input.ordinal <= ordinal)
    }

    pub(super) fn boundary(&self, count: usize) -> Option<(u64, Option<u64>)> {
        if count == 0 || (count > self.model.len() && self.evicted != 0) {
            return None;
        }
        let first = self.model.get(self.model.len().saturating_sub(count))?;
        (first.segment == self.segment).then_some((first.ordinal, first.order))
    }

    /// Identify displayed owners of withdrawn raw inputs without exposing raw message text.
    pub(super) fn withdraw(
        &mut self,
        count: usize,
        aliases: &HashMap<Key, Key>,
        boundary: Option<(u64, Option<u64>)>,
        checkpoint_floor: u64,
    ) -> HashMap<u64, Withdrawal> {
        let mut marked = HashMap::new();
        if count == 0 {
            return marked;
        }
        // Later events cannot supply a missing copy for a boundary already rolled back.
        self.window = self.window.saturating_add(1);
        if self.evicted != 0 {
            if let Some(first) = self.model.front() {
                self.history_floor = self.history_floor.max(first.ordinal);
            }
        } else if self.model.is_empty() {
            return marked;
        }
        let cut = self.model.len().saturating_sub(count);
        let extra = count.saturating_sub(self.model.len());
        if extra != 0 && self.evicted != 0 {
            for (&seq, shown) in &self.displayed {
                if shown.withdrawal.is_none()
                    && (self.model.is_empty() || shown.ordinal < self.history_floor)
                {
                    marked.insert(seq, Withdrawal::Unverified);
                }
            }
            self.evicted = self.evicted.saturating_sub(extra);
        }
        self.match_model_inputs(cut, aliases, &mut marked, |_, _, _| {});
        let cutoff = boundary
            .filter(|(ordinal, _)| {
                self.model
                    .get(cut)
                    .is_some_and(|input| input.ordinal == *ordinal)
            })
            .and_then(|(_, order)| order);
        self.model.truncate(cut);
        if cutoff.is_some() {
            self.model.retain(|input| {
                input.ordinal <= checkpoint_floor
                    || !super::model_cut::removes_accepted(input.accepted_output, cutoff)
            });
        }
        for (seq, &status) in &marked {
            if let Some(shown) = self.displayed.get_mut(seq) {
                shown.withdrawal = Some(status);
            }
        }
        marked
    }

    pub(super) fn confirmed_input_copies(
        &self,
        aliases: &HashMap<Key, Key>,
        checkpoint: &[Value],
    ) -> ConfirmedInputCopies {
        let mut repeated = HashSet::new();
        let mut seen = HashSet::new();
        for input in &self.model {
            if let Some(id) = input.turn.zip(input.id)
                && !seen.insert(id)
            {
                repeated.insert(id);
            }
        }
        let mut copies = HashMap::new();
        let mut marked = HashMap::new();
        self.match_model_inputs(0, aliases, &mut marked, |seq, raw, shown| {
            if let Some(id) = raw.turn.zip(raw.id)
                && !repeated.contains(&id)
                && let Some(witness) = raw.model_content
                && shown.text.is_some()
                && (shown.text == raw.text || shown.text == raw.literal)
            {
                copies.insert(id, (seq, witness));
            }
        });
        copies.retain(|_, (seq, _)| marked.get(seq) == Some(&Withdrawal::Confirmed));
        self.match_checkpoint_inputs(aliases, checkpoint, &mut copies);
        ConfirmedInputCopies {
            by_id: copies,
            original: marked
                .into_iter()
                .filter_map(|(seq, status)| (status == Withdrawal::Confirmed).then_some(seq))
                .collect(),
        }
    }

    fn match_model_inputs(
        &self,
        cut: usize,
        aliases: &HashMap<Key, Key>,
        marked: &mut HashMap<u64, Withdrawal>,
        mut confirm: impl FnMut(u64, Input, Input),
    ) {
        let mut raw_ids = IdIndex::<usize>::new();
        for (n, &raw) in self.model.iter().enumerate() {
            if let Some(id) = raw.id {
                raw_ids
                    .entry((raw.window, raw.turn, id))
                    .or_default()
                    .push((n, raw));
            }
        }
        let mut by_id = IdIndex::<u64>::new();
        let mut by_text = HashMap::<(u64, Option<Key>), Vec<(u64, Input)>>::new();
        for (&seq, stored) in &self.displayed {
            let mut shown = *stored;
            if shown.segment == self.segment
                && let Some(&original) = shown.turn.and_then(|turn| aliases.get(&turn))
            {
                shown.turn = Some(original);
            }
            if shown.withdrawal.is_some() {
                continue;
            }
            if let Some(id) = shown.id.filter(|_| shown.kind == IdKind::Stable) {
                for scope in shown
                    .turn
                    .into_iter()
                    .map(Some)
                    .chain(std::iter::once(None))
                {
                    let identity = (shown.window, scope, id);
                    if raw_ids.contains_key(&identity) {
                        by_id.entry(identity).or_default().push((seq, shown));
                    }
                }
            }
            if shown.spelling == Spelling::Attachment {
                by_text
                    .entry((shown.window, None))
                    .or_default()
                    .push((seq, shown));
            }
            by_text
                .entry((shown.window, shown.text))
                .or_default()
                .push((seq, shown));
        }
        let (reserved, assigned) = self.match_ids(cut, &raw_ids, &by_id, marked, &mut confirm);
        self.mark_unresolved_migration(cut, aliases, &reserved, &assigned, marked);
        let mut pending = HashMap::<_, Vec<(bool, Input)>>::new();
        for (n, &candidate) in self.model.iter().enumerate() {
            if candidate.kind == IdKind::InterAgent || assigned.contains(&n) {
                continue;
            }
            let mut raw = candidate;
            let primary = (raw.window, raw.text);
            if let Some(literal) = raw
                .literal
                .filter(|literal| by_text.contains_key(&(raw.window, Some(*literal))))
            {
                // The same raw diagnostic can represent a failed attachment or a literal user message.
                if by_text.contains_key(&primary) {
                    raw.spelling = Spelling::Ambiguous;
                }
                let mut copy = raw;
                copy.text = Some(literal);
                pending
                    .entry((raw.window, copy.text))
                    .or_default()
                    .push((n >= cut, copy));
            }
            pending.entry(primary).or_default().push((n >= cut, raw));
        }
        for (key, raw) in pending {
            if let Some(shown) = by_text.get(&key) {
                self.match_text(&raw, shown, &reserved, marked, &mut confirm);
            }
        }
    }

    fn match_ids(
        &self,
        cut: usize,
        raw_ids: &IdIndex<usize>,
        by_id: &IdIndex<u64>,
        marked: &mut HashMap<u64, Withdrawal>,
        confirm: &mut impl FnMut(u64, Input, Input),
    ) -> (HashSet<u64>, HashSet<usize>) {
        let mut reserved = HashSet::new();
        let mut assigned = HashSet::new();
        // Resolve scoped IDs before considering an unstamped input with the same ID.
        for scoped in [true, false] {
            for (identity, raw) in raw_ids {
                if identity.1.is_some() != scoped {
                    continue;
                }
                let Some(shown) = by_id.get(identity) else {
                    continue;
                };
                // An unstamped repetition of this same native ID can own the display in any turn.
                let unstamped = raw_ids
                    .get(&(identity.0, None, identity.2))
                    .filter(|_| scoped);
                let dropped = raw.last().is_some_and(|(n, _)| *n >= cut)
                    || unstamped
                        .and_then(|copies| copies.last())
                        .is_some_and(|(n, _)| *n >= cut);
                let confirmed = unstamped.is_none()
                    && raw.len() == 1
                    && shown.len() == 1
                    && scoped
                    && !raw[0].1.inherited
                    && raw[0].1.segment == self.segment
                    && raw[0].1.ordinal.min(shown[0].1.ordinal) >= self.history_floor;
                for &(seq, input) in shown {
                    if reserved.insert(seq) && dropped {
                        if confirmed {
                            confirm(seq, raw[0].1, input);
                        }
                        marked.insert(
                            seq,
                            if confirmed {
                                Withdrawal::Confirmed
                            } else {
                                Withdrawal::Unverified
                            },
                        );
                    }
                }
                assigned.extend(raw.iter().map(|(n, _)| *n));
            }
        }
        (reserved, assigned)
    }

    fn match_text(
        &self,
        raw: &[(bool, Input)],
        shown: &[(u64, Input)],
        reserved: &HashSet<u64>,
        marked: &mut HashMap<u64, Withdrawal>,
        confirm: &mut impl FnMut(u64, Input, Input),
    ) {
        let mut groups = HashMap::<Option<Key>, (Vec<(bool, Input)>, Vec<(u64, Input)>)>::new();
        for &(dropped, input) in raw {
            groups
                .entry(input.turn)
                .or_default()
                .0
                .push((dropped, input));
        }
        let unscoped = groups.contains_key(&None);
        for &(seq, input) in shown {
            if !reserved.contains(&seq) {
                groups.entry(input.turn).or_default().1.push((seq, input));
                if unscoped && input.turn.is_some() {
                    groups.entry(None).or_default().1.push((seq, input));
                }
            }
        }
        for (turn, (raw, mut displayed)) in groups {
            if !raw.iter().any(|(d, _)| *d) {
                continue;
            }
            displayed.sort_unstable_by_key(|(_, item)| item.ordinal);
            let paired = turn.is_some()
                && raw.len() == displayed.len()
                && raw.iter().zip(&displayed).all(|((_, r), (_, d))| {
                    r.text.is_some()
                        && r.spelling != Spelling::Ambiguous
                        && !r.inherited
                        && r.segment == self.segment
                        && r.ordinal.min(d.ordinal) >= self.history_floor
                        && r.adjacent(*d)
                        && r.permits_text(*d)
                })
                && raw
                    .iter()
                    .zip(&displayed)
                    .zip(raw.iter().zip(&displayed).skip(1))
                    .all(|(((_, a), (_, b)), ((_, c), (_, d)))| {
                        a.ordinal.max(b.ordinal) < c.ordinal.min(d.ordinal)
                    });
            if paired {
                for ((dropped, input), (seq, shown)) in raw.into_iter().zip(displayed) {
                    if dropped {
                        confirm(seq, input, shown);
                        marked.entry(seq).or_insert(Withdrawal::Confirmed);
                    }
                }
                continue;
            }
            // Precompute compatible ID classes, including orphan model records, so a long run
            // of identical requests takes linear work after sorting.
            let mut ids = HashSet::new();
            let mut unspecified = false;
            let mut live_input = false;
            for (_, r) in raw.into_iter().filter(|(d, _)| *d) {
                if let Some(id) = r.id {
                    ids.insert(id);
                } else {
                    unspecified = true;
                }
                live_input |= r.kind == IdKind::ModelInput;
            }
            for (seq, d) in displayed {
                if unspecified
                    || d.id.is_none_or(|id| ids.contains(&id))
                    || d.kind == IdKind::GeneratedDisplay
                    || (live_input && d.kind == IdKind::LiveDisplay)
                {
                    marked.insert(seq, Withdrawal::Unverified);
                }
            }
        }
    }
}
