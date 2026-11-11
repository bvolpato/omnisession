# RFC 002: Workspace identity

Status: accepted

Session continuity requires transcript state and repository state.

A captured current workspace records canonical root and current directory, remote fingerprint, worktree, branch, HEAD, dirty-status digest, staged and unstaged diff hashes, untracked manifest, and instruction files. Provider snapshots retain only historical fields exposed by provider stores, often path and branch.

For a fork or resume, OmniSession compares the canonical workspace paths. It checks the current commit, remote fingerprint, and dirty-status digest when the source recorded those values. A typical Claude session needs only the path comparison. A relocated imported workspace requires a matching remote fingerprint.

Codex records Git metadata at session creation. The latest surviving turn determines the launch directory. If that turn selects a different directory, the Git identity from session creation does not apply there. The first listing of a Codex rollout uses its creation directory. Reading or indexing that rollout records its effective directory for later project lists, picker results, and searches. A change to the source file invalidates this observation. Observations apply to the selected Codex home. Cached search shows native transcript text only after OmniSession has indexed it in that home. After an upgrade from a version that did not record the Codex home, run `omni index --provider codex` to rebuild the searchable text.

OmniSession hashes the exact Git remote before storage. Codex can omit user information from a recorded remote. When comparing a Codex fingerprint, OmniSession also checks the local URL with that user information removed. The transport, host, port, and repository path must still match. An SSH path relative to a user’s home directory can refer to different repositories for different accounts. If the recorded fingerprint omits the SSH user, a relocated checkout must also contain the recorded commit. Environment values are never captured. Cross-provider launch reports repository match only when source fields are available; otherwise state is treated as historical or unknown.

Task resolution uses exact normalized workspace identity and an explicit selected task. Modification time may sort candidates but never chooses between logical tasks.
