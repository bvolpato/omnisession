use super::{Digest, Key, Sha256, Value, mcp_part, shell};

pub(super) fn cut(output: &Value) -> Option<Value> {
    let (_, blocks) = output.as_array()?.split_first()?;
    let mut cuts = blocks.iter().enumerate().filter_map(|(index, block)| {
        if block["type"] != "input_text" {
            return None;
        }
        let text = block["text"].as_str()?;
        let mut cut = shell::truncation(text)?;
        cut["index"] = index.into();
        cut["text"] = hex::encode(Sha256::digest(text)).into();
        Some(cut)
    });
    let first = cuts.next()?;
    cuts.next().is_none().then_some(first)
}

pub(super) fn agrees(content: &Value, witness: &Value) -> bool {
    let Some(parts) = content.as_array() else {
        return false;
    };
    let cut = &witness["mcp_part_cut"];
    let Some(expected) = witness["mcp_parts"].as_str() else {
        return false;
    };
    let expected_key = |original| {
        rendered_key(parts, cut, original).is_some_and(|key| hex::encode(key) == expected)
    };
    expected_key(true)
        || (parts
            .iter()
            .any(|part| part["_meta"]["codex/imageDetail"] == "original")
            && expected_key(false))
}

fn rendered_key(parts: &[Value], cut: &Value, original: bool) -> Option<Key> {
    let index = usize::try_from(cut["index"].as_u64()?).ok()?;
    let mut hash = Sha256::new();
    let mut written = 0usize;
    let mut omitted_text = 0usize;
    let mut omitted_audio = 0usize;
    for part in parts {
        let (kind, mut key) = mcp_part(part, original)?;
        if kind == 0 && part["type"] == "text" && part["text"] == "" {
            continue;
        }
        if written == index {
            if kind != 0 {
                return None;
            }
            let encoded;
            let text = if part["type"] == "text" {
                part["text"].as_str()?
            } else {
                encoded = part.to_string();
                &encoded
            };
            if shell::matching_edges(&shell::display_sizes(text), cut, text, 0, true) != Some(true)
            {
                return None;
            }
            hex::decode_to_slice(cut["text"].as_str()?, &mut key).ok()?;
        } else if written > index {
            match kind {
                0 => {
                    omitted_text += 1;
                    continue;
                }
                1 => {
                    omitted_audio += 1;
                    continue;
                }
                _ => {}
            }
        }
        hash.update([kind]);
        hash.update(key);
        written += 1;
    }
    if written <= index {
        return None;
    }
    for (count, name) in [(omitted_text, "text"), (omitted_audio, "audio")] {
        if count > 0 {
            hash.update([0]);
            hash.update(Sha256::digest(format!(
                "[omitted {count} {name} items ...]"
            )));
        }
    }
    Some(hash.finalize().into())
}
