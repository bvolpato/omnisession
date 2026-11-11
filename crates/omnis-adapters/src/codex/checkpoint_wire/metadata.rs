//! Private fields decoded by Codex 0.167.1 alongside a compaction checkpoint.
//! Text is borrowed and lists are validated one entry at a time. None of these fields are rendered.

#![allow(
    dead_code,
    reason = "Fields are read only by the native wire validator"
)]

use serde::{Deserialize, Deserializer, de};
use std::{fmt, marker::PhantomData};

struct Text;

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        <&str>::deserialize(input).map(|_| Self)
    }
}

struct ThreadUuid;

impl<'de> Deserialize<'de> for ThreadUuid {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        uuid::Uuid::parse_str(<&str>::deserialize(input)?)
            .map(|_| Self)
            .map_err(de::Error::custom)
    }
}

struct Entries<T>(PhantomData<T>);

impl<T> Default for Entries<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Entries<T> {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        input.deserialize_seq(Self::default())
    }
}

impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Entries<T> {
    type Value = Self;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of Codex checkpoint entries")
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<T>()?.is_some() {}
        Ok(self)
    }
}

#[derive(Deserialize)]
pub(super) struct TokenUsageRecord {
    thread_id: ThreadUuid,
    turn_id: Text,
    session_id: ThreadUuid,
    root_turn_id: Text,
    response_id: Text,
    usage: TokenUsage,
    turn_token_usage: TokenUsage,
    thread_token_usage: TokenUsage,
}

#[derive(Deserialize)]
pub(super) struct TokenUsageInfo {
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    last_usage_includes_reasoning: Option<bool>,
    model_context_window: Option<i64>,
}

#[derive(Deserialize)]
struct TokenUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    #[serde(default)]
    cache_write_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
    #[serde(default)]
    codex_rollout_budget_units: Option<serde_json::Number>,
}

#[derive(Deserialize)]
pub(super) struct McpResourceOriginCheckpoint {
    origins: Entries<McpResourceOrigin>,
    turns: Entries<Text>,
    #[serde(default)]
    current_turn_id: Option<Text>,
}

#[derive(Deserialize)]
struct McpResourceOrigin {
    call_id: Text,
    #[serde(default)]
    turn_id: Option<Text>,
    tool: Text,
    connector_id: Text,
    #[serde(default)]
    link_id: Option<Text>,
    uri: Text,
    #[serde(default)]
    ambiguous_account: bool,
}

#[derive(Deserialize)]
pub(super) struct RetainedContext {
    verified_answers: Entries<MapOnly<OrderedAnswer>>,
    incomplete: bool,
    #[serde(default)]
    user_messages: Entries<MapOnly<OrderedMessage>>,
    #[serde(default = "missing_user_message_history")]
    user_messages_incomplete: bool,
    #[serde(default)]
    assistant_messages: Entries<MapOnly<OrderedMessage>>,
    #[serde(default)]
    assistant_messages_incomplete: bool,
    #[serde(default)]
    next_order: u64,
    #[serde(default)]
    sender_deliveries: Entries<MapOnly<OrderedSenderMessages>>,
}

fn missing_user_message_history() -> bool {
    true
}

// Codex flattens each ordered record into an object. Read its fields once without buffering unknown values.
struct MapOnly<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOnly<T> {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        input.deserialize_map(Self(PhantomData))
    }
}

impl<'de, T: Deserialize<'de>> de::Visitor<'de> for MapOnly<T> {
    type Value = Self;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an ordered Codex record")
    }

    fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self, M::Error> {
        T::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(self)
    }
}

macro_rules! ordered_record {
    ($name:ident { $($fields:tt)* }) => {
        #[derive(Deserialize)]
        struct $name {
            #[serde(default)]
            revision: Option<Text>,
            #[serde(default)]
            inherited: bool,
            #[serde(default)]
            order: u64,
            $($fields)*
        }
    };
}

ordered_record!(OrderedAnswer {
    turn_id: Text,
    call_id: Text,
    questions: Entries<VerifiedQuestionAnswer>,
});

#[derive(Deserialize)]
struct VerifiedQuestionAnswer {
    question: Text,
    answer: Text,
}

ordered_record!(OrderedMessage {
    turn_id: Text,
    message_id: Option<Text>,
    text: Text,
    complete: bool,
    #[serde(default)]
    origin: UserInputOrigin,
    #[serde(default)]
    phase: Option<MessagePhase>,
});

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserInputOrigin {
    #[default]
    User,
    Heartbeat,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MessagePhase {
    Commentary,
    PartialAnswer,
    FinalAnswer,
}

ordered_record!(OrderedSenderMessages {
    receiver_turn_id: Text,
    receiver_message_id: Text,
    text: Text,
});

#[derive(Deserialize)]
struct SenderUserMessages {
    receiver_turn_id: Text,
    receiver_message_id: Text,
    text: Text,
}

#[derive(Default, Deserialize)]
#[serde(default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "The Codex wire format defines these independent fields."
)]
pub(super) struct CodexHarnessMetadata {
    guardian_sources: Entries<RetainedSource>,
    guardian_review_ids: Entries<Text>,
    guardian_source_order_guidance: bool,
    guardian_retained_omissions: Option<GuardianRetainedOmissions>,
    retained_source: Option<RetainedSource>,
    pub(super) client_authored: bool,
    fallback_token_limit_override: Option<usize>,
    delivered_assistant_message: Option<Text>,
    harness_authored_configuration: bool,
    compaction_model_hash: Option<Text>,
    pub(super) user_input_order: Option<u64>,
    compaction_output: bool,
    pub(super) inherited_user_message: bool,
    // Codex keeps the checkpoint even when it cannot decode MCP attribution.
    mcp_attribution: Option<de::IgnoredAny>,
    sender_user_messages: Option<SenderUserMessages>,
}

#[derive(Deserialize)]
struct GuardianRetainedOmissions {
    user_instructions: bool,
    assistant_context: bool,
}

#[derive(Deserialize)]
struct RetainedSource {
    id: RetainedSourceId,
    revision: Text,
    complete: bool,
}

#[derive(Deserialize)]
struct RetainedSourceId {
    message_id: Text,
    turn_id: Text,
    role: RetainedSourceRole,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetainedSourceRole {
    User,
    Assistant,
}
