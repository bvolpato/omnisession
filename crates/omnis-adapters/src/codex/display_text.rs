//! Codex's visible assistant text in Plan mode.

/// Codex removes citation spans before looking for plan tags on otherwise empty lines.
/// An unfinished plan lasts through the end of the message.
pub(super) fn without_plan(text: &str) -> Option<String> {
    const OPEN: &str = "<proposed_plan>";
    const CLOSE: &str = "</proposed_plan>";
    if !text.contains(OPEN) && !text.contains("<oai-mem-citation>") {
        return None;
    }
    let mut uncited = String::new();
    let text = if visit_uncited(text, |chunk| uncited.push_str(chunk)) {
        uncited.as_str()
    } else {
        text
    };
    let mut visible = String::new();
    let mut in_plan = false;
    let mut saw_plan = false;
    for line in text.split_inclusive('\n') {
        match (in_plan, line.trim()) {
            (false, OPEN) => {
                in_plan = true;
                saw_plan = true;
            }
            (true, CLOSE) => in_plan = false,
            (false, _) => visible.push_str(line),
            (true, _) => {}
        }
    }
    saw_plan.then_some(visible)
}

/// Visit the visible spans only when Codex citation markup is present.
pub(super) fn visit_uncited(text: &str, mut visit: impl FnMut(&str)) -> bool {
    const OPEN: &str = "<oai-mem-citation>";
    const CLOSE: &str = "</oai-mem-citation>";
    let Some((before, mut citation)) = text.split_once(OPEN) else {
        return false;
    };
    visit(before);
    while let Some((_, rest)) = citation.split_once(CLOSE) {
        if let Some((normal, next)) = rest.split_once(OPEN) {
            visit(normal);
            citation = next;
        } else {
            visit(rest);
            break;
        }
    }
    true
}
