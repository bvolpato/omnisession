import type { AgentId } from "./agents";

/** Every session here is invented. The demo never shows a real transcript. */
export type DemoSession = {
  id: string;
  agent: AgentId;
  title: string;
  project: "payments-api" | "checkout-web" | "infra";
  branch: string;
  updated: string;
  messages: number;
  tools: number;
  tokens: string;
  /** Tree glyph before the agent name, when the session belongs to a lineage. */
  tree?: string;
  family?: string;
  /** Extra words that only the conversation text contains. */
  text: string;
  answer: string;
  next: string;
};

export const demoSessions: readonly DemoSession[] = [
  {
    id: "7d1c90ab", agent: "claude-code", title: "Add per-merchant rate limiting to POST /charges", project: "payments-api", branch: "feat/rate-limiter",
    updated: "4h", messages: 41, tools: 63, tokens: "182,440", tree: "┬", family: "limiter", text: "token bucket quota throttle 429 retry-after",
    answer: "The limiter is a token bucket per merchant and route. Requests over the limit get a 429 with Retry-After.",
    next: "Review it for bypass cases.",
  },
  {
    id: "c02e71f4", agent: "grok", title: "Review the new rate limiter for bypass cases", project: "payments-api", branch: "feat/rate-limiter",
    updated: "2h", messages: 12, tools: 9, tokens: "38,905", tree: "├", family: "limiter", text: "header spoofing x-forwarded-for race lua",
    answer: "Two bypasses found: the bucket lives per pod, and the merchant ID is read before auth runs.",
    next: "Share the buckets across pods.",
  },
  {
    id: "4f7fac05", agent: "codex", title: "Share rate-limit buckets across pods with Redis", project: "payments-api", branch: "feat/rate-limiter",
    updated: "38m", messages: 23, tools: 31, tokens: "96,210", tree: "└", family: "limiter", text: "lua script atomic refill redis cluster",
    answer: "Buckets now live in Redis, keyed by merchant and route. One Lua script refills and takes atomically, so concurrent requests can't overspend.",
    next: "Write the README section for the limiter.",
  },
  {
    id: "e3a55d18", agent: "opencode", title: "Load-test per-merchant limits with k6", project: "payments-api", branch: "feat/rate-limiter",
    updated: "9m", messages: 8, tools: 14, tokens: "21,377", tree: "  └", family: "limiter", text: "k6 p99 ramp vus threshold",
    answer: "At 400 virtual users the limiter adds 0.6 ms at p99 and rejects exactly the requests over quota.",
    next: "Turn the k6 run into a CI check.",
  },
  {
    id: "19be4c70", agent: "antigravity", title: "Refactor refunds into an explicit state machine", project: "payments-api", branch: "refactor/refunds",
    updated: "6h", messages: 34, tools: 52, tokens: "140,018", text: "partial refund transition enum ledger",
    answer: "Refund states are an enum with one transition table. Illegal transitions no longer compile.",
    next: "Backfill tests for partial refunds.",
  },
  {
    id: "a8d20f63", agent: "claude-code", title: "Emit refund.updated webhooks to merchants", project: "payments-api", branch: "feat/refund-webhooks",
    updated: "15h", messages: 27, tools: 40, tokens: "88,760", text: "signature hmac delivery retry backoff",
    answer: "Each state change enqueues one signed refund.updated event with retries and backoff.",
    next: "Document the payload.",
  },
  {
    id: "5b90e7c2", agent: "pi", title: "Fix duplicate charges on webhook retries", project: "payments-api", branch: "fix/idempotency",
    updated: "1d", messages: 19, tools: 22, tokens: "54,102", tree: "┬", family: "idempotency", text: "idempotency key race duplicate",
    answer: "The retry path skipped the idempotency check. It now claims the key before it talks to the processor.",
    next: "Add the migration and tests.",
  },
  {
    id: "d41f88e9", agent: "cursor-agent", title: "Add idempotency-key migration and tests", project: "payments-api", branch: "fix/idempotency",
    updated: "22h", messages: 11, tools: 17, tokens: "29,640", tree: "└", family: "idempotency", text: "unique index migration rollback",
    answer: "The migration adds a unique index on merchant and key, built concurrently. Tests cover two racing retries.",
    next: "Check the rollback path.",
  },
  {
    id: "63c7ab1f", agent: "hermes", title: "Investigate p99 latency spike on POST /charges", project: "payments-api", branch: "main",
    updated: "3d", messages: 30, tools: 48, tokens: "117,533", text: "flamegraph connection pool saturation qwen3.8",
    answer: "The spike is pool saturation: every charge opened a second connection for the audit write.",
    next: "Move the audit write to the same transaction.",
  },
  {
    id: "0e9d3b54", agent: "omp", title: "Bump sqlx to 0.8 and fix compile errors", project: "payments-api", branch: "chore/sqlx-0.8",
    updated: "6d", messages: 14, tools: 26, tokens: "41,008", text: "offline cache macros query_as",
    answer: "sqlx 0.8 builds clean. The offline cache is regenerated and checked in.",
    next: "Run the whole suite on Postgres 17.",
  },
  {
    id: "92f4c6d7", agent: "codex", title: "Fix 3-D Secure redirect loop on Safari", project: "checkout-web", branch: "fix/3ds-safari",
    updated: "7h", messages: 22, tools: 35, tokens: "73,914", text: "samesite cookie redirect iframe",
    answer: "Safari dropped the session cookie inside the issuer frame. The return URL now carries a signed state instead.",
    next: "Add a Playwright regression test.",
  },
  {
    id: "b7105ae3", agent: "cursor-ide", title: "Lazy-load the payment method icons", project: "checkout-web", branch: "perf/icons",
    updated: "1d", messages: 9, tools: 12, tokens: "18,220", text: "bundle size sprite intersection",
    answer: "Icons load on first paint of the method list. The entry bundle is 41 KB smaller.",
    next: "Check the skeleton on slow connections.",
  },
  {
    id: "2ca6f90d", agent: "claude-code", title: "Rotate webhook signing secrets without downtime", project: "infra", branch: "main",
    updated: "2d", messages: 25, tools: 33, tokens: "80,451", text: "kms dual sign overlap window",
    answer: "Both secrets sign during a 24 hour overlap. The old one is removed only after every merchant acknowledges.",
    next: "Write the runbook.",
  },
  {
    id: "f85b12c6", agent: "antigravity-desktop", title: "Plan zero-downtime Postgres 17 upgrade", project: "infra", branch: "main",
    updated: "5d", messages: 16, tools: 7, tokens: "35,690", text: "logical replication cutover",
    answer: "Logical replication to a new cluster, a read-only window under 30 seconds, then cut over.",
    next: "Turn the plan into Terraform.",
  },
];
