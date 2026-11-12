use std::collections::HashMap;

use serde::{Deserialize, Deserializer, de};

pub(super) const FIELDS: &[&str] = &[
    "type",
    "id",
    "tool",
    "status",
    "sender_thread_id",
    "receiver_thread_ids",
    "receiver_agents",
    "prompt",
    "model",
    "reasoning_effort",
    "agents_states",
];

#[derive(Deserialize)]
pub(super) struct Call<'a> {
    id: &'a str,
    tool: Tool,
    status: Status,
    sender_thread_id: ThreadId,
    #[serde(default)]
    receiver_thread_ids: Vec<ThreadId>,
    #[serde(default, borrow)]
    receiver_agents: Vec<AgentRef<'a>>,
    #[serde(default)]
    prompt: Option<&'a str>,
    #[serde(default)]
    model: Option<&'a str>,
    #[serde(default)]
    pub(super) reasoning_effort: Option<&'a str>,
    #[serde(default, borrow)]
    agents_states: HashMap<ThreadId, AgentStatus<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Tool {
    SpawnAgent,
    SendInput,
    ResumeAgent,
    Wait,
    CloseAgent,
    SendMessage,
    FollowupTask,
    InterruptAgent,
    ListAgents,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Eq, PartialEq, Hash)]
struct ThreadId(uuid::Uuid);

impl<'de> Deserialize<'de> for ThreadId {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        uuid::Uuid::parse_str(<&str>::deserialize(input)?)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
struct AgentRef<'a> {
    thread_id: ThreadId,
    #[serde(default)]
    agent_nickname: Option<&'a str>,
    #[serde(default, alias = "agent_type")]
    agent_role: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatus<'a> {
    PendingInit,
    Running,
    Interrupted,
    Completed(Option<&'a str>),
    Errored(&'a str),
    Shutdown,
    NotFound,
}
