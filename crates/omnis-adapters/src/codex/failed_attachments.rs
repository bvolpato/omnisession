use serde_json::Value;
use sha2::{Digest, Sha256};

const IMAGE: u8 = b'i';
const AUDIO: u8 = b'a';

/// Match the original request text and all its attachment references.
fn key(text: &[&str], mut paths: Vec<(u8, &str)>) -> Option<[u8; 32]> {
    if paths.is_empty() {
        return None;
    }
    // Legacy display events group local images and audio in separate arrays.
    paths.sort_by_key(|(kind, _)| *kind);
    let mut hash = Sha256::new();
    for (kind, path) in paths {
        hash.update([kind]);
        hash.update(path.len().to_be_bytes());
        hash.update(path.as_bytes());
    }
    hash.update([0]);
    if text.iter().any(|part| !part.trim().is_empty()) {
        for part in text {
            hash.update(part.as_bytes());
        }
    }
    Some(hash.finalize().into())
}

pub(super) fn completed(content: &[Value], text: &[&str]) -> Option<[u8; 32]> {
    let paths = content
        .iter()
        .filter_map(|part| match part["type"].as_str()? {
            "local_image" => Some((IMAGE, part["path"].as_str()?)),
            "local_audio" => Some((AUDIO, part["path"].as_str()?)),
            "image" => reference(part, IMAGE),
            "audio" => reference(part, AUDIO),
            _ => None,
        });
    key(text, paths.collect())
}

pub(super) fn legacy(payload: &Value, text: &str) -> Option<[u8; 32]> {
    let paths = [
        ("local_images", IMAGE),
        ("local_audio", AUDIO),
        ("images", b'I'),
        ("file_ids", b'F'),
        ("audio", b'A'),
    ]
    .into_iter()
    .flat_map(|(field, kind)| {
        payload[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(move |path| Some((kind, path.as_str()?)))
    });
    let recorded_audio = payload["audio_references"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let reference = reference(p, AUDIO)?;
            (reference.0 != b'A' || payload["audio"].as_array().is_none_or(Vec::is_empty))
                .then_some(reference)
        });
    key(&[text], paths.chain(recorded_audio).collect())
}

pub(super) fn model(content: &[Value], text: &[&str]) -> Option<[u8; 32]> {
    let mut paths = Vec::new();
    for (i, part) in content.iter().enumerate() {
        let kind = match part["type"].as_str() {
            Some("input_image") => IMAGE,
            Some("input_audio") => AUDIO,
            Some("input_text") => {
                if let Some(path) = part["text"].as_str().and_then(diagnostic_path) {
                    paths.push(path);
                }
                continue;
            }
            _ => continue,
        };
        let reference = reference(part, kind)?;
        paths.push(local_path(content, i).map_or(reference, |path| (kind, path)));
    }
    let original: Vec<_> = text
        .iter()
        .copied()
        .filter(|part| diagnostic_path(part).is_none())
        .collect();
    key(&original, paths)
}

fn reference(part: &Value, kind: u8) -> Option<(u8, &str)> {
    let (url, inline, file) = if kind == IMAGE {
        ("image_url", b'I', b'F')
    } else {
        ("audio_url", b'A', b'S')
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

/// A model can record both the submitted local path and the uploaded reference for one slot.
#[derive(Clone, Copy)]
pub(super) struct MediaSlot {
    pub(super) reference: [u8; 32],
    pub(super) path: Option<[u8; 32]>,
    pub(super) failed: bool,
}

impl MediaSlot {
    pub(super) fn accepts(self, displayed: Self) -> bool {
        self.reference == displayed.reference || self.path == Some(displayed.reference)
    }
}

pub(super) fn media_slots(parts: &[Value]) -> Option<Box<[MediaSlot]>> {
    let digest = |(kind, value): (u8, &str)| {
        let mut hash = Sha256::new();
        hash.update([kind]);
        hash.update(value.as_bytes());
        hash.finalize().into()
    };
    let mut slots = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let (reference, path, failed) = match part["type"].as_str()? {
            "input_image" | "input_audio" | "image" | "audio" => {
                let kind = if part["type"] == "input_audio" || part["type"] == "audio" {
                    AUDIO
                } else {
                    IMAGE
                };
                (
                    reference(part, kind)?,
                    local_path(parts, i).map(|path| (kind, path)),
                    false,
                )
            }
            "local_image" | "local_audio" => {
                let kind = if part["type"] == "local_audio" {
                    AUDIO
                } else {
                    IMAGE
                };
                (
                    (kind, part["path"].as_str().filter(|p| !p.is_empty())?),
                    None,
                    false,
                )
            }
            "input_text" => {
                let Some(path) = part["text"].as_str().and_then(diagnostic_path) else {
                    continue;
                };
                (path, None, true)
            }
            _ => continue,
        };
        if slots.len() == super::MAX_CANONICAL_EVENTS {
            return None;
        }
        slots.push(MediaSlot {
            reference: digest(reference),
            path: path.map(digest),
            failed,
        });
    }
    (!slots.is_empty()).then(|| slots.into_boxed_slice())
}

pub(super) fn local_path(content: &[Value], index: usize) -> Option<&str> {
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
