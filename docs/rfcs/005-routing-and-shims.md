# RFC 005: Historical provider command routing

Status: superseded by 0.9.0

OmniSession no longer installs provider command aliases, intercepts provider commands, or provides `omni shim`. Running a provider command such as `omp update`, `claude --continue`, or `codex resume` invokes that provider directly and does not read OmniSession task state.

Use `omni` to pick a session, or use explicit commands such as `omni resume SESSION --in PROVIDER` and `omni fork SESSION --in PROVIDER`. `omni task` still tracks exact session bindings by canonical workspace; task selection never depends on recency. See [RFC 002: Workspace identity](002-workspace-identity.md) and [README: Continue or fork](../../README.md#continue-or-fork).
