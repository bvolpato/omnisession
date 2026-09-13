//! Guarded deletion of one Claude Code session.
//!
//! Claude Code has no documented delete command, so deletion removes only data named by the
//! selected session UUID, as Claude Code 2.1 writes it:
//!
//! - `<projects>/<project>/<uuid>.jsonl`, the transcript;
//! - `<projects>/<project>/<uuid>/`, subagent transcripts, tool results, workflow and session memory;
//! - `<config>/file-history/<uuid>/`, file edit backups;
//! - `<config>/session-env/<uuid>/`, session environment;
//! - `<config>/debug/<uuid>.txt`, the debug log.
//!
//! Shared `history.jsonl` prompt lines, `sessions/`, `shell-snapshots/`, `tasks/` (task lists can be
//! shared through `CLAUDE_CODE_TASK_LIST_ID`), project memory, and other sessions stay untouched.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, ErrorKind, Read},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_adapters::{ClaudeAdapter, ProviderAdapter};
use omnis_ir::{Provider, SessionRef};
use serde_json::Value;
use uuid::Uuid;

use crate::claude_import::{self, ClaudeWriteGuard};

/// Transcript head records sampled to confirm the recorded session identity.
const IDENTITY_SAMPLE_RECORDS: usize = 64;
/// Byte budget for the identity sample. A record cut off by the budget is skipped.
const IDENTITY_SAMPLE_BYTES: u64 = 4 * 1024 * 1024;
const STAGING_PREFIX: &str = ".omnisession-delete-";

/// Test replacement for the Claude process-table check.
#[cfg(test)]
type ActiveWriterProbeFn = fn() -> Result<()>;

#[cfg(test)]
thread_local! {
    static ACTIVE_WRITER_PROBE: std::cell::Cell<Option<ActiveWriterProbeFn>> =
        const { std::cell::Cell::new(None) };
}

/// Deletes one discovered Claude Code session and the sidecars named by its UUID.
///
/// Returns the projects-root lock so the caller holds it through its own absence checks.
pub fn delete_session(session: &SessionRef, source_path: &Path) -> Result<ClaudeWriteGuard> {
    delete_session_at(session, source_path, &claude_import::projects_root()?, None)
}

fn delete_session_at(
    session: &SessionRef,
    source_path: &Path,
    projects_root: &Path,
    lock_root: Option<&Path>,
) -> Result<ClaudeWriteGuard> {
    validate_session_identity(session)?;
    ensure_no_active_writer()?;
    let projects_root = safe_projects_root(projects_root)?;
    let guard = claude_import::lock_projects_root(&projects_root, lock_root, None)?;
    ensure_no_active_writer()?;
    let plan = DeletionPlan::resolve(session, source_path, &projects_root)?;
    stage_and_remove(&plan, || verify_absent(session, &projects_root))?;
    Ok(guard)
}

fn ensure_no_active_writer() -> Result<()> {
    #[cfg(test)]
    {
        if let Some(probe) = ACTIVE_WRITER_PROBE.with(std::cell::Cell::get) {
            return probe();
        }
    }
    claude_import::ensure_no_active_claude_process()
}

fn validate_session_identity(session: &SessionRef) -> Result<()> {
    let canonical = Uuid::parse_str(&session.id)
        .ok()
        .map(|id| id.hyphenated().to_string());
    if session.provider != Provider::Claude || canonical.as_deref() != Some(session.id.as_str()) {
        bail!("refusing Claude deletion with invalid session identity");
    }
    Ok(())
}

fn safe_projects_root(projects_root: &Path) -> Result<PathBuf> {
    if !projects_root.is_absolute() {
        bail!("Claude projects root must be absolute for native deletion");
    }
    claude_import::validate_directory_chain(projects_root, "deleting")?;
    fs::canonicalize(projects_root).context("canonicalizing Claude projects root")
}

#[derive(Clone, Copy)]
enum EntryKind {
    File,
    Directory,
}

/// One UUID-named entry and its name inside the private staging directory.
struct OwnedEntry {
    original: PathBuf,
    staged_name: &'static str,
}

/// Exact entries one session owns. The transcript comes first; missing sidecars are omitted.
struct DeletionPlan {
    claude_root: PathBuf,
    entries: Vec<OwnedEntry>,
}

impl DeletionPlan {
    fn resolve(session: &SessionRef, source_path: &Path, projects_root: &Path) -> Result<Self> {
        let transcript = exact_transcript(session, source_path, projects_root)?;
        verify_transcript_identity(&transcript, &session.id)?;
        let claude_root = projects_root
            .parent()
            .context("Claude projects root has no configuration directory")?
            .to_path_buf();
        let id = session.id.as_str();
        let sidecars = [
            (
                transcript.with_file_name(id),
                EntryKind::Directory,
                "session",
            ),
            (
                claude_root.join("file-history").join(id),
                EntryKind::Directory,
                "file-history",
            ),
            (
                claude_root.join("session-env").join(id),
                EntryKind::Directory,
                "session-env",
            ),
            (
                claude_root.join("debug").join(format!("{id}.txt")),
                EntryKind::File,
                "debug.txt",
            ),
        ];
        let mut entries = vec![OwnedEntry {
            original: transcript,
            staged_name: "transcript.jsonl",
        }];
        for (original, kind, staged_name) in sidecars {
            if owned_entry_exists(&original, kind)? {
                entries.push(OwnedEntry {
                    original,
                    staged_name,
                });
            }
        }
        Ok(Self {
            claude_root,
            entries,
        })
    }

    /// Directories whose listings staging or restore changes.
    fn parents(&self) -> BTreeSet<&Path> {
        self.entries
            .iter()
            .filter_map(|entry| entry.original.parent())
            .chain(std::iter::once(self.claude_root.as_path()))
            .collect()
    }
}

/// Accepts only a regular `<projects>/<project>/<uuid>.jsonl` file reached without symlinks.
fn exact_transcript(
    session: &SessionRef,
    source_path: &Path,
    projects_root: &Path,
) -> Result<PathBuf> {
    if !source_path.is_absolute() {
        bail!("Claude transcript path must be absolute");
    }
    let metadata = fs::symlink_metadata(source_path).context("Claude transcript was not found")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Claude transcript is not a safe regular file");
    }
    if let Some(project) = source_path.parent() {
        claude_import::validate_directory_chain(project, "deleting")?;
    }
    let transcript = fs::canonicalize(source_path).context("canonicalizing Claude transcript")?;
    let expected_name = format!("{}.jsonl", session.id);
    let exact = transcript.strip_prefix(projects_root).is_ok_and(|relative| {
        matches!(
            relative.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_), Component::Normal(name)] if *name == OsStr::new(&expected_name)
        )
    });
    if !exact {
        bail!("refusing Claude deletion outside exact `<project>/<uuid>.jsonl` transcript");
    }
    Ok(transcript)
}

/// Requires sampled records that carry `sessionId` to name the selected session, and at least one.
fn verify_transcript_identity(transcript: &Path, id: &str) -> Result<()> {
    let file = fs::File::open(transcript).context("reading Claude transcript identity")?;
    let mut reader = BufReader::new(file.take(IDENTITY_SAMPLE_BYTES));
    let mut line = Vec::new();
    let mut confirmed = false;
    for _ in 0..IDENTITY_SAMPLE_RECORDS {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .context("reading Claude transcript identity")?;
        if read == 0 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match record.get("sessionId") {
            None => {}
            Some(Value::String(recorded)) if recorded == id => confirmed = true,
            Some(_) => bail!("Claude transcript records a different session identity"),
        }
    }
    if !confirmed {
        bail!("cannot verify Claude transcript identity from sampled records");
    }
    Ok(())
}

/// Reports whether an optional sidecar exists. Unsafe shapes are refused rather than skipped.
fn owned_entry_exists(path: &Path, kind: EntryKind) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading `{}`", path.display()));
        }
    };
    if let Some(parent) = path.parent() {
        claude_import::validate_directory_chain(parent, "deleting")?;
    }
    let expected_type = match kind {
        EntryKind::File => metadata.is_file(),
        EntryKind::Directory => metadata.is_dir(),
    };
    if metadata.file_type().is_symlink() || !expected_type {
        bail!(
            "Claude session entry `{}` is not a safe native entry",
            path.display()
        );
    }
    if matches!(kind, EntryKind::Directory) {
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            let file_type = entry
                .context("reading Claude session directory")?
                .file_type();
            if file_type.is_symlink() || (!file_type.is_dir() && !file_type.is_file()) {
                bail!(
                    "Claude session directory `{}` contains an unsafe entry",
                    path.display()
                );
            }
        }
    }
    Ok(true)
}

/// Moves every owned entry into private same-filesystem staging, verifies absence, then removes
/// staged data. A failed move or verification moves everything back.
fn stage_and_remove(plan: &DeletionPlan, verify: impl FnOnce() -> Result<()>) -> Result<()> {
    let staging = create_staging_directory(&plan.claude_root)?;
    let mut staged = Vec::with_capacity(plan.entries.len());
    let result = stage_entries(plan, &staging, &mut staged).and_then(|()| verify());
    if let Err(error) = result {
        return Err(claude_import::combine_rollback_error(
            error,
            restore_staged(plan, &staging, &staged),
            "Claude session deletion",
        ));
    }
    fs::remove_dir_all(&staging).with_context(|| {
        format!(
            "Claude session was deleted but staged data remains in `{}`",
            staging.display()
        )
    })?;
    claude_import::sync_directory(&plan.claude_root)
        .context("syncing Claude configuration directory after deletion")
}

fn stage_entries<'plan>(
    plan: &'plan DeletionPlan,
    staging: &Path,
    staged: &mut Vec<(&'plan Path, PathBuf)>,
) -> Result<()> {
    for entry in &plan.entries {
        let target = staging.join(entry.staged_name);
        fs::rename(&entry.original, &target).with_context(|| {
            format!("staging `{}` for Claude deletion", entry.original.display())
        })?;
        staged.push((entry.original.as_path(), target));
    }
    sync_parents(plan)?;
    for entry in &plan.entries {
        if path_entry_exists(&entry.original)? {
            bail!(
                "staged Claude session entry `{}` still exists",
                entry.original.display()
            );
        }
    }
    Ok(())
}

/// Moves staged entries back in reverse order, never replacing a path recreated meanwhile.
fn restore_staged(plan: &DeletionPlan, staging: &Path, staged: &[(&Path, PathBuf)]) -> Result<()> {
    let mut failures = Vec::new();
    for (original, target) in staged.iter().rev() {
        let restored = match path_entry_exists(original) {
            Ok(false) => fs::rename(target, original)
                .with_context(|| format!("restoring `{}`", original.display())),
            Ok(true) => Err(anyhow!(
                "`{}` was recreated before restore",
                original.display()
            )),
            Err(error) => Err(error),
        };
        if let Err(error) = restored {
            failures.push(format!("{error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!(
            "{}; staged data remains in `{}`",
            failures.join("; "),
            staging.display()
        );
    }
    fs::remove_dir(staging).context("removing Claude deletion staging directory")?;
    sync_parents(plan)
}

fn sync_parents(plan: &DeletionPlan) -> Result<()> {
    for parent in plan.parents() {
        claude_import::sync_directory(parent)
            .with_context(|| format!("syncing `{}`", parent.display()))?;
    }
    Ok(())
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading `{}`", path.display())),
    }
}

/// Creates owner-private staging beside `projects/`, outside the adapter's discovery root.
fn create_staging_directory(claude_root: &Path) -> Result<PathBuf> {
    let staging = claude_root.join(format!("{STAGING_PREFIX}{}", Uuid::new_v4().simple()));
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    builder
        .create(&staging)
        .context("creating private Claude deletion staging directory")?;
    Ok(staging)
}

/// Confirms through an independent Claude adapter that the session is neither listed nor readable.
fn verify_absent(session: &SessionRef, projects_root: &Path) -> Result<()> {
    let adapter = ClaudeAdapter::with_root(projects_root);
    if adapter
        .list_sessions(None)
        .context("verifying staged Claude deletion")?
        .iter()
        .any(|candidate| candidate.session == *session)
    {
        bail!("staged Claude session remains discoverable");
    }
    if adapter.read_session(session).is_ok() {
        bail!("staged Claude session remains readable");
    }
    Ok(())
}

/// Process-table doubles, since developer machines often run Claude while tests run.
#[cfg(all(test, unix))]
mod test_support {
    use anyhow::Result;

    /// Replaces the Claude active-writer probe on the current thread until dropped.
    pub(super) struct ActiveWriterProbe;

    impl ActiveWriterProbe {
        pub(super) fn install(probe: fn() -> Result<()>) -> Self {
            super::ACTIVE_WRITER_PROBE.with(|cell| cell.set(Some(probe)));
            Self
        }
    }

    impl Drop for ActiveWriterProbe {
        fn drop(&mut self) {
            super::ACTIVE_WRITER_PROBE.with(|cell| cell.set(None));
        }
    }

    /// Parses a synthetic process table without Claude.
    pub(super) fn idle() -> Result<()> {
        crate::claude_import::refuse_active_claude_in_macos_ps(
            &crate::macos_ps::ProcessTable::from_outputs(
                "  4000000001 /bin/zsh\n  4000000002 vim claude\n",
                "  4000000001 /bin/zsh\n  4000000002 vim\n",
            ),
        )
    }

    /// Parses a synthetic process table running a native Claude install under a spaced path.
    pub(super) fn claude_running() -> Result<()> {
        crate::claude_import::refuse_active_claude_in_macos_ps(
            &crate::macos_ps::ProcessTable::from_outputs(
                "  4000000001 /bin/zsh\n  4000000003 /Volumes/External Disk/.local/share/claude/versions/2.1.270 --resume synthetic\n",
                "  4000000001 /bin/zsh\n  4000000003 /Volumes/External Disk/.local/share/claude/versions/2.1.270\n",
            ),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::BTreeMap, os::unix::fs::symlink, process::Command};

    use serde_json::json;

    use super::test_support::{self, ActiveWriterProbe};
    use super::*;

    const PROJECT: &str = "-workspace-demo";
    const SUBPROCESS_SESSION: &str = "OMNI_TEST_CLAUDE_DELETE_SESSION";

    #[derive(Debug, PartialEq)]
    enum Node {
        Directory,
        File(Vec<u8>),
        Symlink(PathBuf),
    }

    /// Synthetic Claude configuration directory with a private lock root beside it.
    struct Store {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        claude_root: PathBuf,
        projects_root: PathBuf,
        lock_root: PathBuf,
    }

    impl Store {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary root");
            let root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
            let claude_root = root.join("claude");
            let projects_root = claude_root.join("projects");
            let lock_root = root.join("locks").join("claude");
            fs::create_dir_all(projects_root.join(PROJECT)).expect("Claude project directory");
            Self {
                _temporary: temporary,
                root,
                claude_root,
                projects_root,
                lock_root,
            }
        }

        fn project_dir(&self) -> PathBuf {
            self.projects_root.join(PROJECT)
        }

        /// Writes a transcript and every sidecar Claude Code names by the session UUID.
        fn write_session(&self, id: &str) -> PathBuf {
            let transcript = self.project_dir().join(format!("{id}.jsonl"));
            write(&transcript, transcript_records(id).as_bytes());
            let session = self.project_dir().join(id);
            write(&session.join("subagents/agent-a1b2c3.jsonl"), b"{}\n");
            write(&session.join("subagents/agent-a1b2c3.meta.json"), b"{}");
            write(
                &session.join("tool-results/toolu_01synthetic.txt"),
                b"tool output",
            );
            write(&session.join("session-memory/summary.md"), b"# memory");
            write(
                &self
                    .claude_root
                    .join("file-history")
                    .join(id)
                    .join("0123456789abcdef@v1"),
                b"backup",
            );
            fs::create_dir_all(self.claude_root.join("session-env").join(id))
                .expect("session environment");
            write(
                &self.claude_root.join("debug").join(format!("{id}.txt")),
                b"debug log",
            );
            transcript
        }

        /// Writes Claude state that no single session owns.
        fn write_shared_state(&self, selected: &str, other: &str) {
            let history = [selected, other]
                .map(|id| {
                    format!(
                        "{}\n",
                        json!({"display": "prompt", "project": "/workspace/demo", "sessionId": id})
                    )
                })
                .concat();
            write(&self.claude_root.join("history.jsonl"), history.as_bytes());
            write(&self.claude_root.join("__store.db"), b"synthetic store");
            write(&self.claude_root.join("sessions/4242.json"), b"{}");
            write(
                &self
                    .claude_root
                    .join("shell-snapshots/snapshot-zsh-1-synthetic.sh"),
                b"export SYNTHETIC=1",
            );
            write(
                &self.claude_root.join("tasks").join(selected).join("1.json"),
                b"{}",
            );
            write(&self.project_dir().join("memory/MEMORY.md"), b"# memory");
            write(&self.project_dir().join("sessions-index.json"), b"{}");
            symlink(
                self.claude_root
                    .join("debug")
                    .join(format!("{selected}.txt")),
                self.claude_root.join("debug/latest"),
            )
            .expect("debug latest symlink");
        }

        fn delete(&self, id: &str, transcript: &Path) -> Result<ClaudeWriteGuard> {
            delete_session_at(
                &SessionRef::new(Provider::Claude, id),
                transcript,
                &self.projects_root,
                Some(&self.lock_root),
            )
        }

        fn tree(&self) -> BTreeMap<PathBuf, Node> {
            tree(&self.claude_root)
        }
    }

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
        fs::write(path, contents).expect("fixture file");
    }

    fn transcript_records(recorded_id: &str) -> String {
        [
            json!({
                "type": "file-history-snapshot",
                "messageId": "synthetic-message",
                "snapshot": {},
                "isSnapshotUpdate": false
            }),
            json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {"role": "user", "content": "synthetic question"},
                "uuid": "11111111-1111-4111-8111-111111111111",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": "/workspace/demo",
                "sessionId": recorded_id
            }),
            json!({
                "parentUuid": "11111111-1111-4111-8111-111111111111",
                "isSidechain": false,
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "synthetic answer"}]
                },
                "uuid": "22222222-2222-4222-8222-222222222222",
                "timestamp": "2026-01-01T00:00:01.000Z",
                "cwd": "/workspace/demo",
                "sessionId": recorded_id
            }),
        ]
        .iter()
        .fold(String::new(), |mut document, record| {
            document.push_str(&record.to_string());
            document.push('\n');
            document
        })
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, Node> {
        walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .map(|entry| {
                let entry = entry.expect("tree entry");
                let node = if entry.file_type().is_symlink() {
                    Node::Symlink(fs::read_link(entry.path()).expect("symlink target"))
                } else if entry.file_type().is_dir() {
                    Node::Directory
                } else {
                    Node::File(fs::read(entry.path()).expect("file contents"))
                };
                let relative = entry.path().strip_prefix(root).expect("relative path");
                (relative.to_path_buf(), node)
            })
            .collect()
    }

    /// Expected tree after deleting `id`: every entry except the ones that session owns.
    fn without_owned(tree: BTreeMap<PathBuf, Node>, id: &str) -> BTreeMap<PathBuf, Node> {
        let project = Path::new("projects").join(PROJECT);
        let owned = [
            project.join(format!("{id}.jsonl")),
            project.join(id),
            Path::new("file-history").join(id),
            Path::new("session-env").join(id),
            Path::new("debug").join(format!("{id}.txt")),
        ];
        tree.into_iter()
            .filter(|(path, _)| !owned.iter().any(|owned| path.starts_with(owned)))
            .collect()
    }

    #[test]
    fn deletes_transcript_and_owned_sidecars_only() {
        let _probe = ActiveWriterProbe::install(test_support::idle);
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let other = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);
        store.write_session(&other);
        store.write_shared_state(&selected, &other);
        let before = store.tree();

        store
            .delete(&selected, &transcript)
            .expect("delete selected Claude session");

        assert_eq!(store.tree(), without_owned(before, &selected));
        let sessions = ClaudeAdapter::with_root(&store.projects_root)
            .list_sessions(None)
            .expect("list Claude sessions");
        assert!(sessions.iter().all(|native| native.session.id != selected));
        assert!(sessions.iter().any(|native| native.session.id == other));
    }

    #[test]
    fn active_claude_process_refuses_deletion() {
        let _probe = ActiveWriterProbe::install(test_support::claude_running);
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);
        let before = store.tree();

        let error = store
            .delete(&selected, &transcript)
            .expect_err("running Claude must block deletion");

        assert!(
            error.to_string().contains("while Claude is running"),
            "{error:#}"
        );
        assert_eq!(store.tree(), before);
    }

    #[test]
    fn symlinked_directory_chain_refuses_deletion() {
        let _probe = ActiveWriterProbe::install(test_support::idle);

        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        store.write_session(&selected);
        let outside = store.root.join("outside-project");
        fs::rename(store.project_dir(), &outside).expect("move project outside store");
        symlink(&outside, store.project_dir()).expect("link project directory");
        let before = (store.tree(), tree(&outside));
        let error = store
            .delete(
                &selected,
                &store.project_dir().join(format!("{selected}.jsonl")),
            )
            .expect_err("linked project directory must be refused");
        assert!(error.to_string().contains("unsafe directory"), "{error:#}");
        assert_eq!((store.tree(), tree(&outside)), before);

        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);
        let outside = store.root.join("outside-session-env");
        fs::rename(store.claude_root.join("session-env"), &outside)
            .expect("move session environment outside store");
        symlink(&outside, store.claude_root.join("session-env")).expect("link session environment");
        let before = (store.tree(), tree(&outside));
        let error = store
            .delete(&selected, &transcript)
            .expect_err("linked sidecar parent must be refused");
        assert!(error.to_string().contains("unsafe directory"), "{error:#}");
        assert_eq!((store.tree(), tree(&outside)), before);
    }

    #[test]
    fn mismatched_or_unverifiable_session_identity_refuses_deletion() {
        let _probe = ActiveWriterProbe::install(test_support::idle);
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);

        fs::write(&transcript, transcript_records(&Uuid::new_v4().to_string()))
            .expect("foreign transcript identity");
        let before = store.tree();
        let error = store
            .delete(&selected, &transcript)
            .expect_err("foreign sessionId must be refused");
        assert!(
            error.to_string().contains("different session identity"),
            "{error:#}"
        );
        assert_eq!(store.tree(), before);

        fs::write(&transcript, "{\"type\":\"summary\",\"summary\":\"none\"}\n")
            .expect("unverifiable transcript");
        let before = store.tree();
        let error = store
            .delete(&selected, &transcript)
            .expect_err("unverifiable transcript must be refused");
        assert!(error.to_string().contains("cannot verify"), "{error:#}");
        assert_eq!(store.tree(), before);
    }

    #[test]
    fn failed_readback_restores_original_bytes_and_tree() {
        let _probe = ActiveWriterProbe::install(test_support::idle);
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);
        store.write_shared_state(&selected, &Uuid::new_v4().to_string());
        // A copy in another project keeps the ID discoverable, so staged read-back fails.
        write(
            &store
                .projects_root
                .join("-workspace-copy")
                .join(format!("{selected}.jsonl")),
            transcript_records(&selected).as_bytes(),
        );
        let before = store.tree();

        let error = store
            .delete(&selected, &transcript)
            .expect_err("duplicate must fail staged read-back");

        assert!(
            error.to_string().contains("remains discoverable"),
            "{error:#}"
        );
        assert!(!crate::rollback_failed(&error));
        assert_eq!(store.tree(), before);
    }

    #[test]
    fn rollback_never_replaces_recreated_paths_and_tags_failure() {
        let _probe = ActiveWriterProbe::install(test_support::idle);
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let transcript = store.write_session(&selected);
        let original = fs::read(&transcript).expect("original transcript");
        let session = SessionRef::new(Provider::Claude, &selected);
        let plan = DeletionPlan::resolve(&session, &transcript, &store.projects_root)
            .expect("resolve Claude deletion");

        let error = stage_and_remove(&plan, || {
            fs::write(&transcript, b"recreated\n")?;
            bail!("synthetic read-back failure")
        })
        .expect_err("read-back failure must surface");

        assert!(crate::rollback_failed(&error), "{error:#}");
        assert_eq!(fs::read(&transcript).expect("recreated"), b"recreated\n");
        let staging = fs::read_dir(&store.claude_root)
            .expect("Claude root entries")
            .map(|entry| entry.expect("Claude root entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(STAGING_PREFIX))
            })
            .expect("failed rollback keeps staged data");
        assert_eq!(
            fs::read(staging.join("transcript.jsonl")).expect("staged transcript"),
            original
        );
        assert!(
            store
                .project_dir()
                .join(&selected)
                .join("tool-results/toolu_01synthetic.txt")
                .is_file()
        );
        assert!(
            store
                .claude_root
                .join("debug")
                .join(format!("{selected}.txt"))
                .is_file()
        );
    }

    #[test]
    fn delete_native_session_removes_claude_session_with_isolated_config() {
        let store = Store::new();
        let selected = Uuid::new_v4().to_string();
        let other = Uuid::new_v4().to_string();
        store.write_session(&selected);
        store.write_session(&other);
        store.write_shared_state(&selected, &other);
        let before = store.tree();

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "claude_delete::tests::delete_native_session_subprocess",
                "--nocapture",
            ])
            .env("CLAUDE_CONFIG_DIR", &store.claude_root)
            .env("OMNISESSION_HOME", store.root.join("omnisession"))
            .env(SUBPROCESS_SESSION, &selected)
            .output()
            .expect("run Claude deletion subprocess");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Claude deletion subprocess failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(store.tree(), without_owned(before, &selected));
    }

    /// Child half of the end-to-end test. It does nothing unless the parent isolates Claude state.
    #[test]
    fn delete_native_session_subprocess() {
        let Some(id) = std::env::var_os(SUBPROCESS_SESSION) else {
            return;
        };
        let session = SessionRef::new(Provider::Claude, id.to_str().expect("UTF-8 session ID"));
        let registry = omnis_adapters::AdapterRegistry::with_local_adapters();
        {
            let _probe = ActiveWriterProbe::install(test_support::claude_running);
            let error = crate::delete_native_session(&registry, &session, None)
                .expect_err("running Claude must block deletion");
            assert!(
                error.to_string().contains("while Claude is running"),
                "{error:#}"
            );
            registry
                .read_session(&session)
                .expect("refused deletion keeps the session readable");
        }
        let _probe = ActiveWriterProbe::install(test_support::idle);
        crate::delete_native_session(&registry, &session, None)
            .expect("delete Claude session through the picker callback");
    }
}
