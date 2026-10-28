//! Content-free microbenchmarks over deterministic synthetic session history.

use std::{env, error::Error, fmt::Debug, hint::black_box, path::PathBuf, time::Instant};

use chrono::{DateTime, Utc};
use omnis_core::{
    import_trajectory, plan_semantic_handoff, redact_secrets, render_semantic_handoff,
    session_preview, trajectory_search_document,
};
use omnis_ir::{
    CanonicalSnapshot, EventKind, EventSource, GitState, OmniEvent, Provider, ReplayPolicy,
    SCHEMA_VERSION, Sensitivity, SessionRef, WorkspaceSnapshot,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const OPERATIONS: &[&str] = &[
    "preview",
    "handoff-plan",
    "handoff-render",
    "search",
    "import",
    "redact",
];
const USAGE: &str = "projection-benchmark [--operation all|preview|handoff-plan|handoff-render|search|import|redact] [--events N] [--message-chars N] [--samples N] [--iterations N]";

struct Options {
    operation: String,
    events: usize,
    message_chars: usize,
    samples: usize,
    iterations: u32,
}

impl Options {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut options = Self {
            operation: "all".to_owned(),
            events: 1_000,
            message_chars: 1_024,
            samples: 7,
            iterations: 1,
        };
        let mut args = env::args().skip(1);
        while let Some(argument) = args.next() {
            if matches!(argument.as_str(), "--help" | "-h") {
                println!("{USAGE}");
                std::process::exit(0);
            }
            let value = args.next().ok_or(USAGE)?;
            match argument.as_str() {
                "--operation" => options.operation = value,
                "--events" => options.events = value.parse()?,
                "--message-chars" => options.message_chars = value.parse()?,
                "--samples" => options.samples = value.parse()?,
                "--iterations" => options.iterations = value.parse()?,
                _ => return Err(USAGE.into()),
            }
        }
        if options.events == 0
            || options.message_chars == 0
            || options.samples == 0
            || options.iterations == 0
            || (options.operation != "all" && !OPERATIONS.contains(&options.operation.as_str()))
        {
            return Err(USAGE.into());
        }
        Ok(options)
    }
}

fn synthetic_snapshot(options: &Options) -> CanonicalSnapshot {
    let captured_at: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().expect("fixed timestamp");
    let thread_id = Uuid::from_u128(1);
    let branch_id = Uuid::from_u128(2);
    let padding: String = "Synthetic café α 🦊 history. "
        .chars()
        .cycle()
        .take(options.message_chars)
        .collect();
    let events = (0..options.events)
        .map(|index| {
            let text = format!("Synthetic message {index}: {padding} password=synthetic-value");
            let mut sensitivity = Sensitivity::Normal;
            let mut replay_policy = ReplayPolicy::Contextual;
            let (kind, payload) = if index >= options.events.saturating_sub(8) {
                (EventKind::MessageAssistant, json!({"text": text}))
            } else if index % 31 == 0 {
                replay_policy = ReplayPolicy::HistoricalOnly;
                (
                    EventKind::ProviderEvent,
                    json!({"model":"synthetic-model", "total_tokens":index, "token_usage":"cumulative"}),
                )
            } else if index % 17 == 0 {
                (
                    EventKind::MessageUser,
                    json!({"text":format!("<environment_context>\n{text}\n</environment_context>")}),
                )
            } else if index % 13 == 0 {
                sensitivity = Sensitivity::Secret;
                replay_policy = ReplayPolicy::Secret;
                (EventKind::MessageUser, json!({"text": text}))
            } else if index % 11 == 0 {
                replay_policy = ReplayPolicy::HistoricalOnly;
                (
                    EventKind::ToolCompleted,
                    json!({"call_id":format!("synthetic-{index}"), "output":text, "api_key":"synthetic-secret"}),
                )
            } else if index % 7 == 0 {
                replay_policy = ReplayPolicy::HistoricalOnly;
                (EventKind::PlanUpdated, json!({"next_action":text}))
            } else {
                (
                    if index % 3 == 0 {
                        EventKind::MessageUser
                    } else {
                        EventKind::MessageAssistant
                    },
                    json!({"text":text}),
                )
            };
            let sequence = u64::try_from(index).expect("event index fits u64");
            OmniEvent {
                schema_version: SCHEMA_VERSION.to_owned(),
                event_id: Uuid::from_u128(u128::from(sequence) + 100),
                thread_id,
                branch_id,
                sequence,
                timestamp: Some(captured_at),
                source: EventSource {
                    provider: Provider::Codex,
                    native_session_id: "synthetic-projection-benchmark".to_owned(),
                    provider_version: Some("synthetic".to_owned()),
                    raw_record_type: Some("omnisession.session_metadata".to_owned()),
                },
                kind,
                payload,
                raw_blob_hash: None,
                sensitivity,
                replay_policy,
            }
        })
        .collect();
    CanonicalSnapshot {
        schema_version: SCHEMA_VERSION.to_owned(),
        session: SessionRef::new(Provider::Codex, "synthetic-projection-benchmark"),
        thread_id,
        branch_id,
        title: Some("Synthetic projection benchmark".to_owned()),
        captured_at,
        workspace: WorkspaceSnapshot {
            schema_version: SCHEMA_VERSION.to_owned(),
            captured_at,
            root: PathBuf::from("/workspace/synthetic"),
            current_dir: PathBuf::from("/workspace/synthetic"),
            git: GitState::default(),
            instruction_files: Vec::new(),
            environment_names: Vec::new(),
            available_tools: Vec::new(),
        },
        events,
    }
}

fn debug_digest(value: &impl Debug) -> String {
    hex::encode(Sha256::digest(format!("{value:?}").as_bytes()))
}

fn measure<T: Debug>(name: &str, options: &Options, mut operation: impl FnMut() -> T) -> Value {
    // Warm regex initialization and record the full result outside the timed region.
    let expected_digest = debug_digest(&operation());
    let mut samples = Vec::with_capacity(options.samples);
    for _ in 0..options.samples {
        let started = Instant::now();
        for _ in 0..options.iterations {
            black_box(operation());
        }
        samples.push(started.elapsed().as_nanos() / u128::from(options.iterations));
    }
    assert_eq!(
        debug_digest(&operation()),
        expected_digest,
        "output changed"
    );
    let samples_ns = samples.clone();
    samples.sort_unstable();
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        let lower = samples[middle - 1];
        lower + (samples[middle] - lower) / 2
    } else {
        samples[middle]
    };
    let p95_index = (samples.len() * 95).div_ceil(100) - 1;
    json!({
        "operation":name,
        "samples":options.samples,
        "samples_ns":samples_ns,
        "iterations_per_sample":options.iterations,
        "median_ns":median,
        "p95_ns":samples[p95_index],
        "min_ns":samples[0],
        "output_sha256":expected_digest,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse()?;
    let snapshot = synthetic_snapshot(&options);
    let source_digest = debug_digest(&snapshot);
    let redact_input = format!(
        "{} password=synthetic-value",
        "Synthetic café α 🦊 text. ".repeat(options.message_chars)
    );
    let mut results = Vec::new();
    for &name in OPERATIONS {
        if options.operation != "all" && options.operation != name {
            continue;
        }
        results.push(match name {
            "preview" => measure(name, &options, || session_preview(black_box(&snapshot))),
            "handoff-plan" => measure(name, &options, || {
                plan_semantic_handoff(black_box(&snapshot))
            }),
            "handoff-render" => measure(name, &options, || {
                render_semantic_handoff(black_box(&snapshot))
            }),
            "search" => measure(name, &options, || {
                trajectory_search_document(black_box(&snapshot))
            }),
            "import" => measure(name, &options, || import_trajectory(black_box(&snapshot))),
            "redact" => measure(name, &options, || redact_secrets(black_box(&redact_input))),
            _ => unreachable!("operation is from the fixed list"),
        });
    }
    assert_eq!(debug_digest(&snapshot), source_digest, "source changed");
    println!(
        "{}",
        serde_json::to_string(&json!({
            "events":options.events,
            "message_chars":options.message_chars,
            "redact_input_bytes":redact_input.len(),
            "source_sha256":source_digest,
            "source_unchanged":true,
            "results":results,
        }))?
    );
    Ok(())
}
