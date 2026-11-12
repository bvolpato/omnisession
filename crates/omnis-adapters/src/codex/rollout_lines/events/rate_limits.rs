//! A rate-limit event occupies an ordinal only when Codex can deserialize the full snapshot.
#![expect(
    dead_code,
    reason = "Only the wire types are needed to validate a snapshot"
)]

use serde::Deserialize;
use serde_json::Value;

pub(super) fn valid_token_count(payload: &Value) -> bool {
    TokenCountEvent::deserialize(payload).is_ok()
}

// Codex buffers the fields of this internally tagged event before decoding the snapshot.
// In that representation, an externally tagged unit variant also accepts an empty object.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TokenCountEvent<'a> {
    TokenCount {
        info: Option<serde::de::IgnoredAny>,
        #[serde(borrow)]
        rate_limits: Option<RateLimitSnapshot<'a>>,
    },
}

#[derive(Deserialize)]
struct RateLimitSnapshot<'a> {
    #[serde(borrow)]
    limit_id: Option<&'a str>,
    #[serde(borrow)]
    limit_name: Option<&'a str>,
    #[serde(borrow, default)]
    normal_model_slug: Option<&'a str>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    #[serde(borrow)]
    credits: Option<CreditsSnapshot<'a>>,
    #[serde(borrow)]
    individual_limit: Option<SpendControlLimit<'a>>,
    spend_control_reached: Option<bool>,
    plan_type: Option<AccountPlan>,
    rate_limit_reached_type: Option<RateLimitReachedType>,
}

#[derive(Deserialize)]
struct RateLimitWindow {
    used_percent: f64,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct CreditsSnapshot<'a> {
    has_credits: bool,
    unlimited: bool,
    #[serde(borrow)]
    balance: Option<&'a str>,
}

#[derive(Deserialize)]
struct SpendControlLimit<'a> {
    limit: &'a str,
    used: &'a str,
    remaining_percent: i32,
    resets_at: i64,
}

// The native plan enum accepts future names. The limit-reached enum has no fallback.
#[derive(Deserialize)]
enum AccountPlan {
    #[serde(other)]
    Any,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitReachedType {
    RateLimitReached,
    WorkspaceOwnerCreditsDepleted,
    WorkspaceMemberCreditsDepleted,
    WorkspaceOwnerUsageLimitReached,
    WorkspaceMemberUsageLimitReached,
}
