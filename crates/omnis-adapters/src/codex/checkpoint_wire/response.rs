//! Positional forms of the tagged Codex protocol types used by standalone and checkpoint response items.

use super::super::rollout_lines::name_fields;
use serde_json::Value;

type Variant = (&'static str, &'static [&'static str]);

const CHAT: &str = "internal_chat_message_metadata_passthrough";

// Variant order also determines the integer tags accepted by Serde.
const RESPONSE: &[Variant] = &[
    ("additional_tools", &["type", "id", "role", "tools"]),
    (
        "message",
        &[
            "type",
            "encrypted_content",
            "status",
            "id",
            "role",
            "content",
            "phase",
            CHAT,
        ],
    ),
    (
        "agent_message",
        &["type", "id", "author", "recipient", "content", CHAT],
    ),
    (
        "reasoning",
        &[
            "type",
            "id",
            "summary",
            "content",
            "encrypted_content",
            CHAT,
        ],
    ),
    (
        "local_shell_call",
        &[
            "type",
            "encrypted_content",
            "id",
            "call_id",
            "status",
            "action",
            CHAT,
        ],
    ),
    (
        "function_call",
        &[
            "type",
            "encrypted_content",
            "status",
            "id",
            "name",
            "namespace",
            "arguments",
            "encrypted_function_args",
            "call_id",
            CHAT,
        ],
    ),
    (
        "tool_search_call",
        &[
            "type",
            "encrypted_content",
            "id",
            "call_id",
            "status",
            "execution",
            "arguments",
            CHAT,
        ],
    ),
    (
        "function_call_output",
        &["type", "id", "call_id", "name", "namespace", "output", CHAT],
    ),
    (
        "custom_tool_call",
        &[
            "type",
            "encrypted_content",
            "id",
            "status",
            "call_id",
            "name",
            "namespace",
            "input",
            CHAT,
        ],
    ),
    (
        "custom_tool_call_output",
        &["type", "id", "call_id", "name", "output", CHAT],
    ),
    (
        "tool_search_output",
        &[
            "type",
            "id",
            "call_id",
            "status",
            "execution",
            "tools",
            CHAT,
        ],
    ),
    (
        "web_search_call",
        &["type", "encrypted_content", "id", "status", "action", CHAT],
    ),
    (
        "image_generation_call",
        &[
            "type",
            "encrypted_content",
            "id",
            "status",
            "revised_prompt",
            "result",
            CHAT,
        ],
    ),
    ("compaction", &["type", "id", "encrypted_content", CHAT]),
    ("configuration_update", &["type", "reasoning"]),
    ("compaction_trigger", &["type"]),
    (
        "context_compaction",
        &["type", "id", "encrypted_content", CHAT],
    ),
    ("other", &["type"]),
];

// An empty field list marks a flattened variant that requires an object.
const CONTENT: &[Variant] = &[
    ("input_text", &["type", "text"]),
    ("input_image", &[]),
    ("input_audio", &[]),
    ("output_text", &["type", "text", "annotations", "logprobs"]),
];
const AGENT_CONTENT: &[Variant] = &[
    ("input_text", &["type", "text"]),
    ("encrypted_content", &["type", "encrypted_content"]),
];
const SUMMARY: &[Variant] = &[("summary_text", &["type", "text"])];
const REASONING: &[Variant] = &[
    ("reasoning_text", &["type", "text"]),
    ("text", &["type", "text"]),
];
const WEB: &[Variant] = &[
    ("search", &["type", "query", "queries"]),
    ("open_page", &["type", "url"]),
    ("find_in_page", &["type", "url", "pattern"]),
    ("other", &["type"]),
];
const SHELL: &[Variant] = &[(
    "exec",
    &[
        "type",
        "command",
        "timeout_ms",
        "working_directory",
        "env",
        "user",
    ],
)];

fn variant(value: &mut Value, variants: &[Variant], other: bool) -> Option<&'static str> {
    let tag = value.get("type").or_else(|| value.get(0))?;
    let known = match tag {
        Value::String(tag) => variants.iter().find(|(name, _)| name == tag),
        Value::Number(_) => variants.get(usize::try_from(tag.as_u64()?).ok()?),
        _ => return None,
    };
    let &(name, fields) = known.or_else(|| if other { variants.last() } else { None })?;
    let numeric = tag.is_number();
    if !name_fields(value, fields) {
        return None;
    }
    if numeric {
        value["type"] = name.into();
    }
    Some(name)
}

// Serde decodes unit variants inside an internally tagged item from a string or a single-key object.
fn unit_field(item: &mut Value, field: &str) {
    let Some(Value::Object(map)) = item.get_mut(field) else {
        return;
    };
    if map.len() == 1
        && let Some((name, unit)) = map.iter().next()
        && (unit.is_null() || unit.as_object().is_some_and(serde_json::Map::is_empty))
    {
        item[field] = Value::String(name.clone());
    }
}

fn parts(value: &mut Value, variants: &[Variant]) -> bool {
    value.as_array_mut().is_none_or(|parts| {
        parts.iter_mut().all(|part| {
            let Some(name) = variant(part, variants, false) else {
                return false;
            };
            name != "input_image" || super::super::wire_enums::prepare_image_detail(part)
        })
    })
}

pub(super) fn prepare(item: &mut Value) -> bool {
    if item
        .get("type")
        .or_else(|| item.get(0))
        .is_some_and(|tag| tag == "compaction_summary")
    {
        if let Some(fields) = item.as_array_mut() {
            fields[0] = "compaction".into();
        } else {
            item["type"] = "compaction".into();
        }
    }
    // In a sequence the optional reasoning ciphertext has no Serde default.
    let count = item.as_array().map(Vec::len);
    let Some(name) = variant(item, RESPONSE, true) else {
        return false;
    };
    if name != "other"
        && let Some(chat) = item.get_mut(CHAT)
        && !name_fields(
            chat,
            &[
                "turn_id",
                "create_time",
                "content_item_kinds",
                "content_item_metadata",
            ],
        )
    {
        return false;
    }
    match name {
        "message" => {
            unit_field(item, "phase");
            item.get_mut("content").is_none_or(|v| parts(v, CONTENT))
        }
        "agent_message" => item
            .get_mut("content")
            .is_none_or(|v| parts(v, AGENT_CONTENT)),
        "reasoning" => {
            count.is_none_or(|len| len >= 5)
                && item.get_mut("summary").is_none_or(|v| parts(v, SUMMARY))
                && item.get_mut("content").is_none_or(|v| parts(v, REASONING))
        }
        "web_search_call" => item
            .get_mut("action")
            .is_none_or(|v| v.is_null() || variant(v, WEB, true).is_some()),
        "local_shell_call" => {
            unit_field(item, "status");
            item.get_mut("action").is_none_or(|v| {
                v.as_array().is_none_or(|parts| parts.len() == 6)
                    && variant(v, SHELL, false).is_some()
            })
        }
        "configuration_update" => item
            .get_mut("reasoning")
            .is_none_or(|v| name_fields(v, &["effort"])),
        _ => true,
    }
}
