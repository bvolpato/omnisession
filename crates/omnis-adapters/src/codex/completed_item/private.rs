#![expect(
    dead_code,
    reason = "Serde validates fields omitted from historical metadata"
)]

mod review;

use review::ReviewOutput;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Item<'a> {
    Reasoning {
        id: &'a str,
        #[serde(borrow)]
        summary_text: Vec<&'a str>,
        #[serde(default, borrow)]
        raw_content: Vec<&'a str>,
    },
    HookPrompt {
        id: &'a str,
        #[serde(borrow)]
        fragments: Vec<Fragment<'a>>,
    },
    EnteredReviewMode {
        id: &'a str,
        #[serde(borrow)]
        target: Target<'a>,
        user_facing_hint: &'a str,
    },
    ExitedReviewMode {
        id: &'a str,
        #[serde(borrow)]
        review_output: Option<ReviewOutput<'a>>,
    },
    SubAgentActivity {
        model: Option<&'a str>,
        reasoning_effort: Option<&'a str>,
        id: &'a str,
        kind: Activity,
        agent_thread_id: &'a str,
        agent_path: &'a str,
    },
    ContextCompaction {
        id: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fragment<'a> {
    text: &'a str,
    hook_run_id: &'a str,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Target<'a> {
    UncommittedChanges,
    BaseBranch {
        branch: &'a str,
    },
    Commit {
        sha: &'a str,
        title: Option<&'a str>,
    },
    Custom {
        instructions: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Activity {
    Started,
    Interacted,
    Interrupted,
    Completed,
}

fn agent_path(path: &str) -> bool {
    if path == "/morpheus" {
        return true;
    }
    let mut segments = path.split('/');
    segments.next() == Some("")
        && segments.next() == Some("root")
        && segments.all(|s| {
            !s.is_empty()
                && s != "root"
                && s.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        })
}

pub(super) fn prepare(item: &mut Value) -> bool {
    let Ok(decoded) = Item::deserialize(&*item) else {
        return false;
    };
    let id = match decoded {
        Item::SubAgentActivity {
            id,
            reasoning_effort,
            agent_thread_id,
            agent_path: path,
            ..
        } => {
            if reasoning_effort == Some("")
                || uuid::Uuid::parse_str(agent_thread_id).is_err()
                || !agent_path(path)
            {
                return false;
            }
            id
        }
        Item::ExitedReviewMode { id, review_output } => {
            *item = json!({"type":"ExitedReviewMode","id":id,"text":review::render(review_output.as_ref())});
            return true;
        }
        Item::ContextCompaction { id }
        | Item::Reasoning { id, .. }
        | Item::HookPrompt { id, .. }
        | Item::EnteredReviewMode { id, .. } => id,
    };
    *item = json!({"type":item.get("type").or_else(||item.get(0)), "id":id});
    true
}
