# RFC 002: Workspace identity

Status: accepted

Session continuity requires transcript state and repository state.

A captured current workspace records canonical root and current directory, remote fingerprint, worktree, branch, HEAD, dirty-status digest, staged and unstaged diff hashes, untracked manifest, and instruction files. Provider snapshots retain only historical fields exposed by provider stores, often path and branch.

Remote URLs are hashed before storage. Environment values are never captured. Cross-provider launch reports repository match only when source fields are available; otherwise state is treated as historical or unknown.

Task resolution uses exact normalized workspace identity and an explicit selected task. Modification time may sort candidates but never chooses between logical tasks.
