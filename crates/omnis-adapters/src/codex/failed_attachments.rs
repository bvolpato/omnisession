use serde_json::Value;
use sha2::{Digest, Sha256};

const IMAGE: u8 = b'i';
const AUDIO: u8 = b'a';

/// Match the original request text and the local paths that Codex tried to attach.
fn key(text: &[&str], mut paths: Vec<(u8, &str)>) -> Option<[u8; 32]> {
    if paths.is_empty() {
        return None;
    }
    // Legacy display events group local images and audio in separate arrays.
    paths.sort_unstable();
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
    let paths = content.iter().filter_map(|part| {
        let kind = match part["type"].as_str()? {
            "local_image" => IMAGE,
            "local_audio" => AUDIO,
            _ => return None,
        };
        Some((kind, part["path"].as_str()?))
    });
    key(text, paths.collect())
}

pub(super) fn legacy(payload: &Value, text: &str) -> Option<[u8; 32]> {
    let paths = [("local_images", IMAGE), ("local_audio", AUDIO)]
        .into_iter()
        .flat_map(|(field, kind)| {
            payload[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |path| Some((kind, path.as_str()?)))
        });
    key(&[text], paths.collect())
}

pub(super) fn model(content: &[Value], text: &[&str]) -> Option<[u8; 32]> {
    let mut paths: Vec<_> = text
        .iter()
        .filter_map(|part| diagnostic_path(part))
        .collect();
    if paths.is_empty() {
        return None;
    }
    // Successful local attachments in the same request retain their path in the opening tag.
    paths.extend(content.windows(2).filter_map(|pair| {
        let (prefix, kind) = match pair[1]["type"].as_str()? {
            "input_image" => ("<image name=[Image #", IMAGE),
            "input_audio" => ("<audio name=[Audio #", AUDIO),
            _ => return None,
        };
        let (number, path) = pair[0]["text"]
            .as_str()?
            .strip_prefix(prefix)?
            .strip_suffix("\">")?
            .split_once("] path=\"")?;
        (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some((kind, path))
    }));
    let original: Vec<_> = text
        .iter()
        .copied()
        .filter(|part| diagnostic_path(part).is_none())
        .collect();
    key(&original, paths)
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
