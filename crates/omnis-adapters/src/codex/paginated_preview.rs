//! Project completed Codex items before choosing the records to show in a large preview.

use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{BufReader, Seek},
};

use anyhow::{Result, ensure};
use omnis_ir::{EventKind, ReplayPolicy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::rollout_lines::{Admission, RolloutLines, Wire};
use super::{CodexHistory, CodexSession, EventBuilder, RolloutStamp, declares_history_base};
use crate::support::{
    JsonLineLocation, JsonlRecord, MAX_PREVIEW_EDGE_BYTES, MAX_PROVIDER_RECORDS,
    MAX_STREAMED_TRANSCRIPT_FILE_SIZE, estimated_event_bytes, read_json_line_at, string_at,
    visit_streamed_json_records_with_offsets,
};

pub(super) fn read(session: &CodexSession, limit: usize) -> Result<EventBuilder> {
    let mut reader = BufReader::new(File::open(&session.path)?);
    let metadata = reader.get_ref().metadata()?;
    let stamp = RolloutStamp::new(&metadata);
    let mut lines = RolloutLines::default();
    let mut source_index = 0;
    let mut inherited = false;
    let mut remaining = 4 * MAX_PREVIEW_EDGE_BYTES;
    let mut history = CodexHistory::new(session.project_path.clone());
    history.paginated = true;
    let mut exact = Some((history, session.event_builder()));
    let mut independent = Some(IndependentItems::default());
    let source_bytes = session.id.len() + session.cli_version.as_ref().map_or(0, String::len);

    let oversized = visit_streamed_json_records_with_offsets(
        &mut reader,
        MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
        |loc, line| {
            let (mut record, terminated) = match line {
                JsonlRecord::Complete(record) => (record, true),
                JsonlRecord::Unterminated(record) => (record, false),
                JsonlRecord::Malformed => {
                    lines.malformed();
                    return Ok(());
                }
            };
            let index = source_index;
            source_index += 1;
            if index == 0 {
                ensure!(
                    record["type"] == "session_meta"
                        && record["payload"]["history_mode"] == "paginated"
                        && string_at(&record["payload"], &[&["id"], &["session_id"]])
                            == Some(session.id.as_str()),
                    "selected Codex rollout has a different session header"
                );
                inherited = declares_history_base(&record);
                if inherited {
                    independent = None;
                }
                lines.start_segment(&record);
            }

            let admission = lines.prepare(&mut record, terminated, Some(true));
            if admission == Admission::Skip {
                return Ok(());
            }
            if let Some(items) = &mut independent
                && !items.observe(session, &record, loc, index, source_bytes, admission)
            {
                independent = None;
            }
            if let Some((history, builder)) = &mut exact
                && !project_within_budget(
                    history,
                    builder,
                    &record,
                    index,
                    admission,
                    source_bytes,
                    &mut remaining,
                )?
            {
                exact = None;
            }
            Ok(())
        },
    )?;

    let builder = if oversized > 0 {
        unavailable(session, Some(lines))
    } else if let Some((mut history, mut builder)) = exact {
        history.rollout_lines = lines;
        history.source_ordinal = source_index;
        history.compact(&mut builder);
        if inherited {
            session.report_unread_history_base(&mut builder);
        }
        if history.omitted_tool_events > 0 {
            notice(session, &mut builder, history.omitted_tool_events);
        }
        history.finish(&mut builder);
        retain_edges(session, &mut builder, limit, source_bytes);
        builder
    } else if let Some(items) = independent {
        items.preview(session, &mut reader, limit, lines, source_index)?
    } else {
        unavailable(session, Some(lines))
    };
    if stamp.is_none()
        || RolloutStamp::new(&reader.get_ref().metadata()?) != stamp
        || RolloutStamp::read(&session.path) != stamp
    {
        return Ok(unavailable(session, None));
    }
    Ok(builder)
}

fn retain_edges(
    session: &CodexSession,
    builder: &mut EventBuilder,
    limit: usize,
    source_bytes: usize,
) {
    let selected = select_edges(builder.events(), limit, |event| {
        estimated_event_bytes(&event.payload) + source_bytes
    });
    let omitted = builder.events().len() - selected.len();
    if omitted > 0 {
        let keep: HashSet<_> = selected
            .into_iter()
            .map(|index| builder.events()[index].sequence)
            .collect();
        builder.retain_events(|event| keep.contains(&event.sequence));
        notice(session, builder, omitted);
    }
}

fn project_within_budget(
    history: &mut CodexHistory,
    builder: &mut EventBuilder,
    record: &Value,
    index: u64,
    admission: Admission,
    source_bytes: usize,
    remaining: &mut usize,
) -> Result<bool> {
    // Unknown record types and token counts can contain large fields that the projector discards.
    let event = record["type"] == "event_msg";
    let opaque = !matches!(
        record["type"].as_str(),
        Some(
            "session_meta"
                | "event_msg"
                | "turn_context"
                | "compacted"
                | "response_item"
                | "inter_agent_communication"
        )
    ) || (event && record["payload"]["type"] == "token_count")
        || (event
            && record["payload"]["type"] == "item_completed"
            && admission == Admission::ModelOnly);
    if !opaque {
        let directory = history
            .project_path
            .as_deref()
            .map_or(0, |p| p.as_os_str().len());
        let copies = record["payload"]["replacement_history"]
            .as_array()
            .map_or(1, |rows| rows.len() + 1);
        let cost = estimated_event_bytes(record)
            .saturating_add(copies.saturating_mul(source_bytes.saturating_add(directory)));
        let Some(budget) = remaining.checked_sub(cost) else {
            return Ok(false);
        };
        *remaining = budget;
    }
    let before = builder.checkpoint();
    history.source_ordinal = index;
    history.push_admitted(builder, record, admission)?;
    let events = builder.events();
    let start = events.partition_point(|event| event.sequence < before);
    let cost = events[start..].iter().fold(0usize, |cost, event| {
        cost.saturating_add(estimated_event_bytes(&event.payload))
            .saturating_add(source_bytes)
    });
    let Some(budget) = remaining.checked_sub(cost) else {
        return Ok(false);
    };
    *remaining = budget;
    if history
        .source_ordinal
        .is_multiple_of(super::TOOL_COMPACTION_RECORD_INTERVAL as u64)
    {
        history.compact(builder);
    }
    Ok(true)
}

fn select_edges<T>(items: &[T], limit: usize, bytes: impl Fn(&T) -> usize) -> BTreeSet<usize> {
    let mut selected = BTreeSet::new();
    let mut add = |indices: &mut dyn Iterator<Item = usize>| {
        let mut remaining = MAX_PREVIEW_EDGE_BYTES;
        for (index, cost) in indices
            .map(|i| (i, bytes(&items[i])))
            .filter(|(_, n)| *n <= MAX_PREVIEW_EDGE_BYTES)
            .take(limit / 2)
        {
            let Some(rest) = remaining.checked_sub(cost) else {
                break;
            };
            remaining = rest;
            selected.insert(index);
        }
    };
    add(&mut (0..items.len()));
    add(&mut (0..items.len()).rev());
    selected
}

#[derive(Default)]
struct IndependentItems {
    ids: HashSet<[u8; 32]>,
    items: Vec<PublicItem>,
}

struct PublicItem {
    location: JsonLineLocation,
    index: u64,
    bytes: usize,
}

impl IndependentItems {
    fn observe(
        &mut self,
        session: &CodexSession,
        record: &Value,
        location: JsonLineLocation,
        index: u64,
        source_bytes: usize,
        admission: Admission,
    ) -> bool {
        let p = &record["payload"];
        if admission == Admission::ModelOnly
            && record["type"] == "event_msg"
            && p["type"] == "item_completed"
        {
            return true;
        }
        match (record["type"].as_str(), p["type"].as_str()) {
            (Some("session_meta"), _) => location.offset == 0 && !declares_history_base(record),
            (Some("progress" | "turn_context" | "inter_agent_communication"), _)
            | (Some("response_item"), Some("message" | "agent_message"))
            | (
                Some("event_msg"),
                Some(
                    "task_started"
                    | "turn_started"
                    | "task_complete"
                    | "turn_complete"
                    | "turn_aborted"
                    | "token_count"
                    | "thread_settings_applied"
                    | "user_message"
                    | "agent_message",
                ),
            ) => true,
            (Some("event_msg"), Some("item_completed")) => {
                let item = &p["item"];
                let Some(id) = item["id"].as_str().filter(|id| !id.is_empty()) else {
                    return false;
                };
                if p["thread_id"].as_str() != Some(session.id.as_str())
                    || p["turn_id"].as_str().is_none_or(str::is_empty)
                    || !matches!(item["type"].as_str(), Some("UserMessage" | "AgentMessage"))
                    || self.ids.len() == MAX_PROVIDER_RECORDS
                    || !self.ids.insert(Sha256::digest(id.as_bytes()).into())
                {
                    return false;
                }
                self.items.push(PublicItem {
                    location,
                    index,
                    bytes: estimated_event_bytes(record).saturating_add(source_bytes),
                });
                true
            }
            _ => false,
        }
    }

    fn preview(
        self,
        session: &CodexSession,
        reader: &mut BufReader<File>,
        limit: usize,
        lines: RolloutLines,
        end: u64,
    ) -> Result<EventBuilder> {
        let selected = select_edges(&self.items, limit, |item| item.bytes);
        let mut position = reader.stream_position()?;
        let mut line = Vec::new();
        let mut builder = session.event_builder();
        let mut history = CodexHistory::new(None);
        history.paginated = true;
        let reported_loss = lines.invalid_messages > 0
            || lines.invalid_checkpoints > 0
            || lines.skipped > 0
            || lines.gap
            || lines.contested;
        history.rollout_lines = lines;
        for &i in &selected {
            let item = &self.items[i];
            let mut record = read_json_line_at(reader, item.location, &mut position, &mut line)?;
            if history
                .rollout_lines
                .prepare_wire(&mut record, true, Some(true))
                == Wire::Invalid
            {
                return Ok(unavailable(session, None));
            }
            history.source_ordinal = item.index;
            history.push_admitted(&mut builder, &record, Admission::Complete)?;
        }
        history.source_ordinal = end;
        history.finish(&mut builder);
        let omitted = self.items.len() - selected.len();
        if omitted > 0 || !reported_loss {
            notice(session, &mut builder, omitted.max(1));
        }
        Ok(builder)
    }
}

fn notice(session: &CodexSession, builder: &mut EventBuilder, omitted: usize) {
    builder.push(
        EventKind::ProviderEvent,
        json!({"omitted_events":omitted}),
        session.updated_at,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.preview_limit".into()),
        None,
    );
}

fn unavailable(session: &CodexSession, lines: Option<RolloutLines>) -> EventBuilder {
    let mut builder = session.event_builder();
    builder.push(
        EventKind::ProviderEvent,
        json!({"omitted_events":1,"native_history_unverified":true,
        "reason":"The bounded Codex preview could not verify the active history."}),
        session.updated_at,
        ReplayPolicy::HistoricalOnly,
        Some("omnisession.preview_limit".into()),
        None,
    );
    if let Some(lines) = lines.filter(|lines| lines.contested) {
        let mut history = CodexHistory::new(None);
        history.rollout_lines = lines;
        history.finish(&mut builder);
    }
    builder
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const DATE: &str = "2026-02-01T10:15:00Z";

    #[test]
    fn sparse_preview_keeps_the_last_valid_positional_message() {
        const ID: &str = "019f0000-0000-7000-8000-000000000279";
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(format!("rollout-{ID}.jsonl"));
        let header = json!({"type":"session_meta","ordinal":0,"timestamp":DATE,"payload":{"id":ID,"history_mode":"paginated"}});
        let message = |content: Value| {
            json!({"type":"event_msg","ordinal":1,"timestamp":DATE,"payload":["item_completed",ID,"turn",
                ["UserMessage","request",null,content]]})
        };
        let published = message(json!([["text","published caption",[]],
            {"type":"image","file_id":"private-image"}]));
        let mut invalid = message(json!([["text","malformed revision",[]],
            {"type":"local_image","path":false}]));
        let stale = message(json!([["text", "earlier revision"]]));
        let mut contested = message(json!([["text", "contested revision"]]));
        let mut documentary = json!({"timestamp":DATE,"type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"future_limit":"private metadata"}}});
        let mut resumed = message(json!([["text", "post-gap caption"]]));
        resumed["payload"][3][1] = "final".into();
        let mut model = json!({"type":"response_item","timestamp":DATE,"payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"model only"}],"padding":"x".repeat(MAX_PREVIEW_EDGE_BYTES)}});
        for (padding_rows, unfamiliar) in [(0, false), (5, false), (5, true)] {
            let mut file = File::create(&path).unwrap();
            writeln!(file, "{header}\n{published}").unwrap();
            for index in 0..padding_rows {
                model["ordinal"] = (index + 2).into();
                serde_json::to_writer(&mut file, &model).unwrap();
                writeln!(file).unwrap();
            }
            invalid["ordinal"] = (padding_rows + 2).into();
            documentary["ordinal"] = (padding_rows + 3).into();
            documentary["payload"]["type"] = if unfamiliar {
                "future_event"
            } else {
                "token_count"
            }
            .into();
            contested["ordinal"] = documentary["ordinal"].clone();
            resumed["ordinal"] = (padding_rows + 5).into();
            writeln!(
                file,
                "{invalid}\n{stale}\n{{broken\n{documentary}\n{contested}\n{resumed}"
            )
            .unwrap();
            drop(file);
            let session = CodexSession::parse_metadata_path(&path).unwrap();
            let builder = session.preview_events().unwrap();
            let events = builder.events();
            let messages: Vec<_> = events
                .iter()
                .filter_map(|e| e.payload["text"].as_str())
                .collect();
            if unfamiliar {
                assert!(messages.is_empty());
            } else {
                assert_eq!(messages, ["published caption", "post-gap caption"]);
            }
            let omissions: Vec<_> = events
                .iter()
                .filter_map(|e| {
                    Some((
                        e.source.raw_record_type.as_deref()?,
                        e.payload["omitted_events"].as_u64()?,
                    ))
                })
                .collect();
            let mut expected = vec![
                ("omnisession.codex_invalid_message", 1),
                ("omnisession.codex_rollout_line", 3),
            ];
            if unfamiliar {
                expected.insert(0, ("omnisession.preview_limit", 1));
            }
            assert_eq!(omissions, expected);
            assert!(events.iter().any(
                |e| e.payload["ordinal_gap"] == true && e.payload["contested_ordinal"] == true
            ));
        }
    }
}
