use super::{Key, Sha256, Value, WITNESS, json, single_key};
use sha2::Digest;

fn decimal<T: std::str::FromStr + ToString>(text: &str) -> Option<T> {
    let n = text.parse::<T>().ok()?;
    (n.to_string() == text).then_some(n)
}

fn body_key(text: &str) -> String {
    let key: Key = Sha256::digest(text).into();
    hex::encode(single_key(0, key))
}

pub(super) fn output(output: &Value) -> Option<Value> {
    let text = output.as_str()?;
    let mut original_tokens = None;
    let (exit, mut body, lines, timeout) = if let Some(raw) = text.strip_prefix("Exit code: ") {
        let (code, tail) = raw.split_once('\n')?;
        let exit: i32 = decimal(code)?;
        let (duration, tail) = tail.strip_prefix("Wall time: ")?.split_once(" seconds\n")?;
        let seconds: f32 = decimal(duration)?;
        if !seconds.is_finite()
            || seconds < 0.0
            || ((seconds * 10.0).round() / 10.0).to_string() != duration
        {
            return None;
        }
        let (lines, tail) = if let Some(rest) = tail.strip_prefix("Total output lines: ") {
            let (n, rest) = rest.split_once('\n')?;
            (Some(decimal::<usize>(n)?), rest)
        } else {
            (None, tail)
        };
        (exit, tail.strip_prefix("Output:\n")?, lines, Some(seconds))
    } else {
        let tail = if let Some(raw) = text.strip_prefix("Chunk ID: ") {
            let (chunk, tail) = raw.split_once('\n')?;
            if chunk.is_empty() {
                return None;
            }
            tail
        } else {
            text
        };
        let (time, tail) = tail.split_once('\n')?;
        if !super::mcp_time(time) {
            return None;
        }
        let (exit, tail) = if let Some(rest) = tail.strip_prefix("Process exited with code ") {
            let (code, tail) = rest.split_once('\n')?;
            (Some(decimal::<i32>(code)?), tail)
        } else {
            (None, tail)
        };
        let (process, tail) =
            if let Some(rest) = tail.strip_prefix("Process running with session ID ") {
                let (id, tail) = rest.split_once('\n')?;
                (Some(decimal::<i32>(id)?), tail)
            } else {
                (None, tail)
            };
        let tail = if let Some(rest) = tail.strip_prefix("Original token count: ") {
            let (count, rest) = rest.split_once('\n')?;
            original_tokens = Some(decimal::<u64>(count)?);
            rest
        } else {
            tail
        };
        let body = tail.strip_prefix("Output:\n")?;
        if let Some(id) = process {
            return Some(json!({"running": id}));
        }
        (exit?, body, None, None)
    };
    let mut result = json!({"code":exit,"body":body_key(body)});
    let mut timeout_bytes = 0;
    if let Some(seconds) = timeout
        && exit == 124
        && let Some((ms, rest)) = body
            .strip_prefix("command timed out after ")
            .and_then(|s| s.split_once(" milliseconds\n"))
        && let Some(ms) = decimal::<u64>(ms)
        && ((std::time::Duration::from_millis(ms).as_secs_f32() * 10.0).round() / 10.0).to_bits()
            == seconds.to_bits()
    {
        timeout_bytes = body.len() - rest.len();
        body = rest;
        result["timeout_body"] = body_key(body).into();
    }
    if let Some(mut cut) = if timeout.is_none() {
        unified_truncation(body, original_tokens)
    } else {
        truncation(body)
    } {
        if let Some(lines) = lines {
            cut["lines"] = lines
                .saturating_sub(usize::from(result["timeout_body"].is_string()))
                .into();
        }
        cut["timeout_bytes"] = timeout_bytes.into();
        result["cut"] = cut;
    }
    Some(result)
}

fn unified_truncation(body: &str, original_tokens: Option<u64>) -> Option<Value> {
    let warning = body.strip_prefix("Warning: truncated output (original token count: ");
    let Some((count, tail)) = warning.and_then(|s| s.split_once(")\n")) else {
        return truncation(body);
    };
    let tokens = decimal::<u64>(count)?;
    if let Some((n, snippet)) = tail
        .strip_prefix("Total output lines: ")
        .and_then(|s| s.split_once("\n\n"))
    {
        let mut cut = truncation(snippet)?;
        cut["lines"] = decimal::<usize>(n)?.into();
        return Some(cut);
    }
    let (notice, snippet) = if let Some(snippet) = tail.strip_prefix('\n') {
        (None, snippet)
    } else {
        let (line, snippet) = tail.split_once("\n\n")?;
        (Some(omission_count(line)?), snippet)
    };
    let mut cut = truncation(snippet)?;
    cut["capture"] = json!({"notice":notice,"tokens":tokens,"original":original_tokens});
    Some(cut)
}

pub(super) fn truncation(body: &str) -> Option<Value> {
    let mut markers = body.match_indices('…').filter_map(|(start, separator)| {
        let (marker, suffix) = body[start + separator.len()..].split_once('…')?;
        let (digits, unit) = marker.split_once(' ')?;
        let count = decimal::<usize>(digits)?;
        (count != 0 && matches!(unit, "chars truncated" | "tokens truncated")).then_some((
            &body[..start],
            suffix,
            count,
            unit,
        ))
    });
    let (prefix, suffix, count, unit) = markers.next()?;
    if markers.next().is_some() {
        return None;
    }
    Some(
        json!({"prefix":body_key(prefix),"suffix":body_key(suffix),"left":prefix.len(),"right":suffix.len(),"count":count,"tokens":unit=="tokens truncated"}),
    )
}

const CAPTURE_EDGE_BYTES: usize = 512 * 1024;

fn omission_count(marker: &str) -> Option<u64> {
    decimal::<u64>(
        marker
            .strip_prefix("... ")?
            .strip_suffix(" bytes omitted ...")?,
    )
    .filter(|n| *n > 0)
}

fn captured_bytes(text: &str) -> Option<u64> {
    if text.len() <= 2 * CAPTURE_EDGE_BYTES {
        return None;
    }
    let total = text.matches(char::REPLACEMENT_CHARACTER).count();
    let mut offset = 0;
    let mut replacements = 0;
    let mut omitted = None;
    for (before, _) in text.match_indices("\n... ") {
        replacements += text[offset..before]
            .matches(char::REPLACEMENT_CHARACTER)
            .count();
        offset = before;
        let Some(head_excess) = before.checked_sub(CAPTURE_EDGE_BYTES) else {
            continue;
        };
        let Some((marker, tail)) = text[before + 1..].split_once('\n') else {
            continue;
        };
        let Some(count) = omission_count(marker) else {
            continue;
        };
        let Some(tail_excess) = tail.len().checked_sub(CAPTURE_EDGE_BYTES) else {
            continue;
        };
        // CapturedStream decodes the retained byte slices with from_utf8_lossy.
        if head_excess <= 2 * replacements && tail_excess <= 2 * (total - replacements) {
            if omitted.is_some() {
                return None;
            }
            omitted = Some(count);
        }
    }
    omitted
}

fn capture_matches(capture: &Value, text: &str, prefix: &str, suffix: &str) -> bool {
    let Some(omitted) = captured_bytes(text) else {
        return false;
    };
    let marker = format!("... {omitted} bytes omitted ...");
    let inline = prefix.contains(&marker) || suffix.contains(&marker);
    let expected_tokens = if capture["original"].is_null() {
        Some((text.len() as u64).div_ceil(4))
    } else {
        omitted
            .checked_add(2 * CAPTURE_EDGE_BYTES as u64)
            .map(|bytes| bytes.div_ceil(4))
    };
    expected_tokens.is_some()
        && capture["tokens"].as_u64() == expected_tokens
        && (capture["original"].is_null() || capture["original"] == capture["tokens"])
        && if inline {
            capture["notice"].is_null()
        } else {
            capture["notice"] == omitted
        }
}

pub(super) fn display_sizes(text: &str) -> Value {
    json!({"bytes":text.len(),"chars":text.chars().count(),"lines":text.lines().count()})
}

fn truncated_matches(display: &Value, cut: &Value, source: Option<&str>) -> Option<bool> {
    let sizes = &display[WITNESS]["shell_sizes"];
    let text = source.or_else(|| display["output"].as_str())?;
    let header = usize::try_from(cut["timeout_bytes"].as_u64()?).ok()?;
    matching_edges(sizes, cut, text, header, source.is_some())
}

pub(super) fn matching_edges(
    sizes: &Value,
    cut: &Value,
    text: &str,
    header: usize,
    full_source: bool,
) -> Option<bool> {
    let bytes = usize::try_from(sizes["bytes"].as_u64()?).ok()?;
    let chars = usize::try_from(sizes["chars"].as_u64()?).ok()?;
    let left = usize::try_from(cut["left"].as_u64()?).ok()?;
    let right = usize::try_from(cut["right"].as_u64()?).ok()?;
    let count = usize::try_from(cut["count"].as_u64()?).ok()?;
    let prefix = text.get(..left)?;
    let suffix = text.get(text.len().checked_sub(right)?..)?;
    if let Some(capture) = cut.get("capture")
        && !capture_matches(capture, text, prefix, suffix)
    {
        return Some(false);
    }
    let lc = prefix.chars().count();
    let rc = suffix.chars().count();
    let edge = super::super::super::MAX_TOOL_STRING_CHARACTERS / 2;
    if !full_source && chars > edge * 2 && (lc > edge || rc > edge) {
        return Some(false);
    }
    let omitted = bytes.checked_sub(left.checked_add(right)?)?;
    let source_bytes = bytes + header;
    let head = left + header;
    let next_width = text.get(left..)?.chars().next()?.len_utf8();
    let last_width = text
        .get(..text.len() - right)?
        .chars()
        .next_back()?
        .len_utf8();
    let valid_count = (0..=6).any(|slack| {
        let budget = head + right + slack;
        if budget >= source_bytes
            || !(head..head + next_width).contains(&(budget / 2))
            || !(right..right + last_width).contains(&(budget - budget / 2))
        {
            return false;
        }
        if cut["tokens"] == true {
            budget.is_multiple_of(4) && (source_bytes - budget).div_ceil(4) == count
        } else {
            chars.checked_sub(lc + rc) == Some(count)
        }
    });
    Some(
        omitted > 0
            && valid_count
            && (cut["lines"].is_null() || cut["lines"] == sizes["lines"])
            && cut["prefix"] == body_key(prefix)
            && cut["suffix"] == body_key(suffix),
    )
}

pub(super) fn precedes_terminal_display(display: &Value, witness: &Value) -> bool {
    matches!(display["status"].as_str(), Some("completed" | "failed"))
        && witness["running"]
            .as_i64()
            .is_some_and(|id| display["process_id"].as_str().and_then(decimal::<i64>) == Some(id))
}

pub(super) fn agrees(display: &Value, witness: &Value, source: Option<&str>) -> bool {
    if witness["running"].is_number() {
        return false;
    }
    let code = &witness["code"];
    let expected = &display[WITNESS]["body"];
    if !display["exit_code"].is_null() && display["exit_code"] != *code {
        return false;
    }
    let status_agrees = match display["status"].as_str() {
        Some("completed") => *code == 0,
        Some("failed") => *code != 0,
        _ => true,
    };
    status_agrees
        && expected.is_string()
        && (*expected == witness["body"]
            || *expected == witness["timeout_body"]
            || truncated_matches(display, &witness["cut"], source) == Some(true))
}
