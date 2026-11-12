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
        let (code, tail) = tail
            .strip_prefix("Process exited with code ")?
            .split_once('\n')?;
        let exit: i32 = decimal(code)?;
        let tail = if let Some(rest) = tail.strip_prefix("Original token count: ") {
            let (count, rest) = rest.split_once('\n')?;
            decimal::<u64>(count)?;
            rest
        } else {
            tail
        };
        (exit, tail.strip_prefix("Output:\n")?, None, None)
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
    let mut lines = lines;
    if timeout.is_none()
        && let Some(warning) =
            body.strip_prefix("Warning: truncated output (original token count: ")
        && let Some((count, tail)) = warning.split_once(")\nTotal output lines: ")
        && decimal::<u64>(count).is_some()
        && let Some((line_count, tail)) = tail.split_once("\n\n")
        && let Some(line_count) = decimal::<usize>(line_count)
    {
        lines = Some(line_count);
        body = tail;
    }
    if let Some(mut cut) = truncation(body) {
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

fn truncation(body: &str) -> Option<Value> {
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

pub(super) fn display_sizes(text: &str) -> Value {
    json!({"bytes":text.len(),"chars":text.chars().count(),"lines":text.lines().count()})
}

fn truncated_matches(display: &Value, cut: &Value) -> Option<bool> {
    let sizes = &display[WITNESS]["shell_sizes"];
    let bytes = usize::try_from(sizes["bytes"].as_u64()?).ok()?;
    let chars = usize::try_from(sizes["chars"].as_u64()?).ok()?;
    let left = usize::try_from(cut["left"].as_u64()?).ok()?;
    let right = usize::try_from(cut["right"].as_u64()?).ok()?;
    let count = usize::try_from(cut["count"].as_u64()?).ok()?;
    let text = display["output"].as_str()?;
    let prefix = text.get(..left)?;
    let suffix = text.get(text.len().checked_sub(right)?..)?;
    let lc = prefix.chars().count();
    let rc = suffix.chars().count();
    let edge = super::super::super::MAX_TOOL_STRING_CHARACTERS / 2;
    if chars > edge * 2 && (lc > edge || rc > edge) {
        return Some(false);
    }
    let omitted = bytes.checked_sub(left.checked_add(right)?)?;
    let timeout_bytes = usize::try_from(cut["timeout_bytes"].as_u64()?).ok()?;
    let source_bytes = bytes + timeout_bytes;
    let head = left + timeout_bytes;
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

pub(super) fn agrees(display: &Value, witness: &Value) -> bool {
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
            || truncated_matches(display, &witness["cut"]) == Some(true))
}
