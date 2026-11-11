use serde_json::Value;
use sha2::{Digest, Sha256};

type Key = [u8; 32];
const IMAGE: u8 = 0;
const AUDIO: u8 = 1;
const IMAGE_URL: u8 = 2;
const AUDIO_URL: u8 = 3;
const IMAGE_FILE: u8 = 4;
const AUDIO_FILE: u8 = 5;
const GROUPS: usize = 6;

fn local_kind(kind: u8) -> u8 {
    match kind {
        IMAGE | IMAGE_URL | IMAGE_FILE => IMAGE,
        _ => AUDIO,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MediaShape {
    pub(super) signature: Key,
    pub(super) local: usize,
    pub(super) has_caption: bool,
    pub(super) failed: bool,
}

#[derive(Clone, Copy, Debug)]
struct MediaSlot {
    reference: Key,
    path: Option<Key>,
    kind: u8,
    failed: bool,
}

impl MediaSlot {
    fn new((kind, value): (u8, &str), path: Option<&str>, failed: bool) -> Self {
        let digest = |kind: u8, value: &str| {
            let mut hash = Sha256::new();
            hash.update([kind]);
            hash.update(value.as_bytes());
            hash.finalize().into()
        };
        Self {
            kind,
            reference: digest(kind, value),
            path: path.map(|p| digest(local_kind(kind), p)),
            failed,
        }
    }

    fn matches(self, other: Self) -> bool {
        self.reference == other.reference
            || self.path == Some(other.reference)
            || other.path == Some(self.reference)
    }
}

/// Each slot keeps digests of the references that the provider recorded for that attachment.
#[derive(Clone, Debug)]
pub(super) struct AttachmentReceipt {
    shape: MediaShape,
    slots: Box<[MediaSlot]>,
    grouped: Option<[bool; 2]>,
}

impl AttachmentReceipt {
    fn new(
        text: &[&str],
        slots: Vec<MediaSlot>,
        grouped: Option<[bool; 2]>,
        model: bool,
    ) -> Option<Self> {
        if slots.is_empty() {
            return None;
        }
        let mut counts = [0_usize; 2];
        let mut local = 0;
        for slot in &slots {
            counts[usize::from(local_kind(slot.kind))] += 1;
            local += usize::from(matches!(slot.kind, IMAGE | AUDIO) || slot.path.is_some());
        }
        let mut hash = Sha256::new();
        for count in counts {
            hash.update(count.to_be_bytes());
        }
        let caption = text
            .iter()
            .copied()
            .filter(|t| !model || diagnostic_path(t).is_none());
        let has_caption = caption.clone().any(|t| !t.trim().is_empty());
        if has_caption {
            for part in caption {
                hash.update(part.as_bytes());
            }
        }
        let shape = MediaShape {
            signature: hash.finalize().into(),
            local,
            has_caption,
            failed: slots.iter().any(|s| s.failed),
        };
        Some(Self {
            shape,
            slots: slots.into_boxed_slice(),
            grouped,
        })
    }

    pub(super) fn bucket(&self) -> Key {
        self.shape.signature
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn shape(&self) -> MediaShape {
        self.shape
    }

    pub(super) fn anchors(&self) -> impl Iterator<Item = (Key, Option<Key>)> + '_ {
        self.slots.iter().map(|s| (s.reference, s.path))
    }

    pub(super) fn spelling(&self) -> Key {
        let mut hash = Sha256::new();
        hash.update([1]);
        hash.update(self.bucket());
        for slot in &self.slots {
            hash.update(slot.path.unwrap_or(slot.reference));
        }
        hash.finalize().into()
    }

    pub(super) fn matches(&self, other: &Self) -> bool {
        if self.bucket() != other.bucket() || self.len() != other.len() {
            return false;
        }
        if let Some(ordered) = self.grouped {
            self.matches_groups(other, ordered)
        } else if let Some(ordered) = other.grouped {
            other.matches_groups(self, ordered)
        } else {
            self.slots
                .iter()
                .zip(&other.slots)
                .all(|(a, b)| a.matches(*b))
        }
    }

    // Legacy events record local paths in separate arrays. Preserve order within each array.
    fn matches_groups(&self, other: &Self, ordered: [bool; 2]) -> bool {
        let group = |kind| {
            usize::from(match kind {
                IMAGE_FILE if ordered[usize::from(IMAGE)] => IMAGE_URL,
                AUDIO_FILE if ordered[usize::from(AUDIO)] => AUDIO_URL,
                _ => kind,
            })
        };
        let mut groups: [Vec<Key>; GROUPS] = Default::default();
        for slot in &self.slots {
            groups[group(slot.kind)].push(slot.reference);
        }
        let mut consumed = [0; GROUPS];
        for slot in &other.slots {
            let wire = group(slot.kind);
            let local = group(local_kind(slot.kind));
            let by_wire = groups[wire].get(consumed[wire]) == Some(&slot.reference);
            let by_path = slot
                .path
                .is_some_and(|path| groups[local].get(consumed[local]) == Some(&path));
            match (by_wire, by_path) {
                (true, false) => consumed[wire] += 1,
                (false, true) => consumed[local] += 1,
                _ => return false,
            }
        }
        true
    }
}

pub(super) fn completed(parts: &[Value], text: &[&str]) -> Option<AttachmentReceipt> {
    let mut slots = Vec::new();
    for part in parts {
        let kind = match part["type"].as_str()? {
            "local_image" | "image" => IMAGE,
            "local_audio" | "audio" => AUDIO,
            "text" if part["text"].is_string() => continue,
            "skill" | "mention" if part["name"].is_string() && part["path"].is_string() => continue,
            _ => return None,
        };
        let reference = if matches!(part["type"].as_str(), Some("local_image" | "local_audio")) {
            (kind, part["path"].as_str().filter(|s| !s.is_empty())?)
        } else {
            reference(part, kind)?
        };
        if slots.len() == super::MAX_CANONICAL_EVENTS {
            return None;
        }
        slots.push(MediaSlot::new(reference, None, false));
    }
    AttachmentReceipt::new(text, slots, None, false)
}

pub(super) fn model(content: &[Value], text: &[&str]) -> Option<AttachmentReceipt> {
    let mut slots = Vec::new();
    for (i, part) in content.iter().enumerate() {
        if !super::checkpoint_wire::message_part(part) {
            return None;
        }
        let (reference, path, failed) = match part["type"].as_str()? {
            "input_image" | "input_audio" => {
                let kind = if part["type"] == "input_image" {
                    IMAGE
                } else {
                    AUDIO
                };
                (reference(part, kind)?, local_path(content, i), false)
            }
            "input_text" => {
                let Some(path) = part["text"].as_str().and_then(diagnostic_path) else {
                    continue;
                };
                (path, None, true)
            }
            _ => return None,
        };
        if slots.len() == super::MAX_CANONICAL_EVENTS {
            return None;
        }
        slots.push(MediaSlot::new(reference, path, failed));
    }
    AttachmentReceipt::new(text, slots, None, true)
}

pub(super) fn legacy(payload: &Value, text: &str) -> Option<AttachmentReceipt> {
    let mut groups: [Vec<MediaSlot>; GROUPS] = Default::default();
    let mut count = 0_usize;
    let mut add = |reference: (u8, &str)| {
        if reference.1.is_empty() || count == super::MAX_CANONICAL_EVENTS {
            return None;
        }
        count += 1;
        groups[usize::from(reference.0)].push(MediaSlot::new(reference, None, false));
        Some(())
    };
    let array = |field: &str| match &payload[field] {
        Value::Null => Some(&[][..]),
        Value::Array(items) => Some(items.as_slice()),
        _ => None,
    };
    for (field, kind) in [
        ("local_images", IMAGE),
        ("local_audio", AUDIO),
        ("images", IMAGE_URL),
        ("file_ids", IMAGE_FILE),
        ("audio", AUDIO_URL),
    ] {
        for part in array(field)? {
            add((kind, part.as_str()?))?;
        }
    }
    for part in array("audio_references")? {
        let r = reference(part, AUDIO)?;
        if r.0 != AUDIO_URL || array("audio")?.is_empty() {
            add(r)?;
        }
    }
    let mut slots = Vec::with_capacity(count);
    let mut ordered = [false; 2];
    for (kind, inline_kind, file_kind, field) in [
        (IMAGE, IMAGE_URL, IMAGE_FILE, "image_order"),
        (AUDIO, AUDIO_URL, AUDIO_FILE, "audio_order"),
    ] {
        let k = usize::from(kind);
        slots.append(&mut groups[k]);
        let mut inline = std::mem::take(&mut groups[usize::from(inline_kind)]).into_iter();
        let mut file = std::mem::take(&mut groups[usize::from(file_kind)]).into_iter();
        let order = array(field)?;
        if order.is_empty() {
            slots.extend(inline);
            slots.extend(file);
            continue;
        }
        ordered[k] = true;
        for part in order {
            slots.push(match part.as_str()? {
                "inline" => inline.next()?,
                "file" => file.next()?,
                _ => return None,
            });
        }
        if inline.len() + file.len() != 0 {
            return None;
        }
    }
    AttachmentReceipt::new(&[text], slots, Some(ordered), false)
}

fn reference(part: &Value, kind: u8) -> Option<(u8, &str)> {
    let (url, inline, file) = if kind == IMAGE {
        ("image_url", IMAGE_URL, IMAGE_FILE)
    } else {
        ("audio_url", AUDIO_URL, AUDIO_FILE)
    };
    part[url]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|v| (inline, v))
        .or_else(|| {
            part["file_id"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(|v| (file, v))
        })
}

fn local_path(content: &[Value], index: usize) -> Option<&str> {
    let prefix = match content[index]["type"].as_str()? {
        "input_image" => "<image name=[Image #",
        "input_audio" => "<audio name=[Audio #",
        _ => return None,
    };
    let tag = content.get(index.checked_sub(1)?)?;
    let (number, path) = tag["text"]
        .as_str()?
        .strip_prefix(prefix)?
        .strip_suffix("\">")?
        .split_once("] path=\"")?;
    (tag["type"] == "input_text"
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && !path.is_empty())
    .then_some(path)
}

// The protocol converts a failed local attachment into a separate input text part.
fn diagnostic_path(text: &str) -> Option<(u8, &str)> {
    for (prefix, delimiter, kind) in [
        ("Codex could not read the local image at `", "`: ", IMAGE),
        ("Codex could not read the local audio at `", "`: ", AUDIO),
        ("Image located at `", "` is invalid: ", IMAGE),
    ] {
        if let Some((path, error)) = text
            .strip_prefix(prefix)
            .and_then(|tail| tail.rsplit_once(delimiter))
        {
            return (!path.is_empty() && !error.is_empty()).then_some((kind, path));
        }
    }
    if let Some(tail) = text.strip_prefix("Codex cannot attach audio at `") {
        let path =
            tail.strip_suffix("`: unsupported audio format; use wav, mp3, m4a, webm, or ogg.")?;
        return (!path.is_empty()).then_some((AUDIO, path));
    }
    let (path, mime) = text
        .strip_prefix("Codex cannot attach image at `")?
        .strip_suffix("`.")?
        .rsplit_once("`: unsupported image `")?;
    (!path.is_empty() && !mime.is_empty()).then_some((IMAGE, path))
}
