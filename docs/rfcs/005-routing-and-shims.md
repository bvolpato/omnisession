# RFC 005: Routing and shims

Status: accepted

Transparent routing is opt-in. Ordinary commands pass directly to provider binaries. Only documented resume and continue forms may be intercepted.

Resolution order:

1. No selected OmniSession task for exact workspace: pass through.
2. Current branch head already targets invoked provider: resume bound native session.
3. Current branch head targets another provider: create semantic handoff branch.
4. Multiple tasks with none selected: stop and require selection.

`OMNI_BYPASS=1` always bypasses routing. Real binary resolution excludes shim directory and supports explicit per-provider override variables. Final handoff uses process replacement to preserve TTY behavior.
