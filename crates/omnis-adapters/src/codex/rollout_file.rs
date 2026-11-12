//! Read the physical representation of a Codex JSONL rollout without modifying the source.

use std::{
    cell::{Cell, OnceCell},
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read, Seek},
    path::{Path, PathBuf},
};

use crate::source_file::ObservedSourceFile;
use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use omnis_ir::SourceRevision;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CodexHistoryMode;
use super::workspace_check::{WorkspaceScan, same_thread};
use super::workspace_damage::{self, DamagedWorkspace};
use crate::support::WorkspaceGap;
use crate::support::{
    JsonLineLocation, JsonLineSkips, JsonLinesLimits, MAX_PREVIEW_TAIL_SIZE,
    MAX_STREAMED_TRANSCRIPT_FILE_SIZE, MAX_STREAMED_TRANSCRIPT_LINE_SIZE, MAX_TRANSCRIPT_LINE_SIZE,
    json_lines_prefix, visit_json_reader, visit_json_reader_with_locations,
};

pub(super) const SAMPLE_RECORDS: usize = 1_024;

pub(super) fn jsonl_file_name(path: &Path) -> Option<&Path> {
    let plain = match path.extension()?.to_str()? {
        "gz" | "zst" => Path::new(path.file_stem()?),
        "jsonl" => Path::new(path.file_name()?),
        _ => return None,
    };
    (plain.extension()? == "jsonl").then_some(plain)
}

impl super::CodexAdapter {
    pub const FULL_READ_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

    /// Hashes a verified rollout through its current or previously recorded length.
    /// Returns no evidence if the requested prefix exceeds the full-read budget or the file changes during the read.
    #[must_use]
    pub fn rollout_prefix_hash(
        path: &Path,
        expected: SourceRevision,
        bytes: Option<u64>,
    ) -> Option<(u64, [u8; 32])> {
        Self::hash_name_source(path, expected, bytes, None)
    }

    /// Binds the name to the complete ordinary rollout or the original metadata of a native fork.
    #[must_use]
    pub fn rollout_name_anchor(
        path: &Path,
        expected: SourceRevision,
        session_id: &str,
    ) -> Option<(u64, [u8; 32])> {
        Self::hash_name_source(path, expected, None, Some(session_id))
    }

    fn hash_name_source(
        path: &Path,
        expected: SourceRevision,
        bytes: Option<u64>,
        session_id: Option<&str>,
    ) -> Option<(u64, [u8; 32])> {
        jsonl_file_name(path)?;
        if expected.contents.is_none()
            || omnis_core::may_reach_network(path)
            || !fs::symlink_metadata(path).ok()?.is_file()
        {
            return None;
        }
        let canonical = fs::canonicalize(path).ok()?;
        if omnis_core::may_reach_network(&canonical) {
            return None;
        }
        let mut file = File::open(&canonical).ok()?;
        let metadata = file.metadata().ok()?;
        let source = ObservedSourceFile::from_open_file(&canonical, &file, &metadata)?;
        if source.physical_identity.is_none() || !expected.matches_file_contents(source.revision) {
            return None;
        }
        if let Some(id) = session_id
            && !compressed(path)
        {
            if let Some(header) = fork_name_header(&file, path, id, expected) {
                source.ensure_unchanged(&canonical, path, &file).ok()?;
                return Some((
                    u64::try_from(header.len()).ok()?,
                    Sha256::digest(header).into(),
                ));
            }
            file.rewind().ok()?;
        }
        let length = bytes.unwrap_or(metadata.len());
        if !(1..=Self::FULL_READ_SOURCE_BYTES).contains(&length) || length > metadata.len() {
            return None;
        }
        let mut reader = (&file).take(length);
        let mut hash = Sha256::new();
        let mut chunk = [0; 16 * 1024];
        while reader.limit() > 0 {
            let read = reader.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            hash.update(&chunk[..read]);
        }
        source.ensure_unchanged(&canonical, path, &file).ok()?;
        Some((length, hash.finalize().into()))
    }
}

fn fork_name_header(
    file: &File,
    path: &Path,
    id: &str,
    revision: SourceRevision,
) -> Option<Vec<u8>> {
    let mut header = Vec::new();
    BufReader::new(file.take(MAX_TRANSCRIPT_LINE_SIZE))
        .read_until(b'\n', &mut header)
        .ok()?;
    if !header.ends_with(b"\n") {
        return None;
    }
    let record = serde_json::from_slice(&header).ok()?;
    let observed = super::ObservedRollout::read(path)?;
    if !revision.matches_file_contents(observed.revision) {
        return None;
    }
    let session = super::CodexSession::parse_metadata_record(path.to_owned(), &record, observed)?;
    let parent = session.forked_from_id?;
    (same_thread(&session.id, id) && !same_thread(&parent.to_string(), id)).then_some(header)
}

pub(super) fn compressed(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "zst" || extension == "gz")
}

pub(super) fn representation_priority(path: &Path) -> u8 {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("jsonl") => 2,
        Some("zst") => 1,
        _ => 0,
    }
}

fn physical_file(path: &Path) -> Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        bail!("Codex rollout must be a regular file");
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_STREAMED_TRANSCRIPT_FILE_SIZE {
        bail!("Codex rollout exceeds safe streaming limit or is not a regular file");
    }
    Ok(file)
}

// Resolving one fork can open several rollouts while selecting and verifying archival copies.
// Charge all reads on the synchronous discovery thread to the same search budget.
thread_local! {
    static SEARCH_BYTES: Cell<Option<u64>> = const { Cell::new(None) };
}

pub(super) fn within_search_budget<T>(
    remaining: &mut u64,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SEARCH_BYTES.set(self.0);
        }
    }
    let _restore = Restore(SEARCH_BYTES.replace(Some(*remaining)));
    let result = run();
    *remaining = SEARCH_BYTES.get().expect("active search budget");
    anyhow::ensure!(*remaining != 0, "workspace search budget exhausted");
    result
}

struct SearchReader<R>(R);

impl<R: Read> Read for SearchReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = SEARCH_BYTES.get().expect("active search budget");
        if buf.is_empty() {
            return Ok(0);
        }
        if remaining == 0 {
            return Err(std::io::Error::other("workspace search budget exhausted"));
        }
        let limit = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let count = self.0.read(&mut buf[..limit])?;
        SEARCH_BYTES.set(Some(remaining - count as u64));
        Ok(count)
    }
}

pub(super) fn open(path: &Path) -> Result<Box<dyn Read>> {
    let file = physical_file(path)?;
    if let Some(remaining) = SEARCH_BYTES.get() {
        if compressed(path) {
            // A decoder can consume a large encoded stream before it returns any output.
            let encoded = file.metadata()?.len();
            SEARCH_BYTES.set(Some(remaining.saturating_sub(encoded)));
            anyhow::ensure!(encoded < remaining, "workspace search budget exhausted");
        }
        return Ok(Box::new(SearchReader(decoder(path, file)?)));
    }
    decoder(path, file)
}

fn decoder(path: &Path, file: File) -> Result<Box<dyn Read>> {
    let reader: Box<dyn Read> = match path.extension().and_then(|ext| ext.to_str()) {
        Some("gz") => Box::new(MultiGzDecoder::new(file)),
        Some("zst") => {
            let mut decoder = zstd::stream::read::Decoder::new(file)?;
            // The cold-rollout compressor uses standard frames. Bound decoder memory independently of output size.
            decoder.window_log_max(27)?;
            Box::new(decoder)
        }
        _ => Box::new(file),
    };
    Ok(reader)
}

#[derive(Debug)]
pub(super) struct CompressedHeaderError;

impl std::fmt::Display for CompressedHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reading compressed Codex rollout header")
    }
}

pub(super) fn header(path: &Path) -> Result<Vec<Value>> {
    if !compressed(path) && SEARCH_BYTES.get().is_none() {
        return json_lines_prefix(path, 1);
    }
    // Listing thousands of archives should consume only their first line.
    let mut line = Vec::new();
    BufReader::new(open(path)?.take(MAX_TRANSCRIPT_LINE_SIZE + 1))
        .read_until(b'\n', &mut line)
        .context(CompressedHeaderError)?;
    if u64::try_from(line.len())? > MAX_TRANSCRIPT_LINE_SIZE {
        bail!("Codex rollout header exceeds safe line limit");
    }
    Ok(serde_json::from_slice(&line).ok().into_iter().collect())
}

#[derive(Default)]
pub(super) struct NativeReadBudget {
    encoded: u64,
    decoded: u64,
}

impl NativeReadBudget {
    pub(super) fn contains_prefix(
        &mut self,
        path: &Path,
        end: u64,
        check: impl FnOnce(&mut dyn Read) -> Result<bool>,
    ) -> Result<bool> {
        let file = physical_file(path)?;
        let length = file.metadata()?.len();
        let remaining = MAX_STREAMED_TRANSCRIPT_FILE_SIZE - self.decoded;
        if end > remaining {
            return Ok(false);
        }
        if !compressed(path) {
            if end > length {
                return Ok(false);
            }
            self.decoded += end;
            let mut prefix = file.take(end);
            return Ok(check(&mut prefix)? && prefix.limit() == 0);
        }
        if length > MAX_STREAMED_TRANSCRIPT_FILE_SIZE - self.encoded {
            return Ok(false);
        }
        self.encoded += length;
        let mut reader = decoder(path, file)?.take(remaining + 1);
        let mut prefix = reader.by_ref().take(end);
        if !check(&mut prefix)? || prefix.limit() != 0 {
            return Ok(false);
        }
        // Native Codex must also open the suffix beyond an otherwise valid bookmark.
        let bytes = end + std::io::copy(&mut reader, &mut std::io::sink())?;
        if bytes > remaining {
            return Ok(false);
        }
        self.decoded += bytes;
        Ok(true)
    }
}

pub(super) const LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
    records: usize::MAX,
    line_bytes: MAX_STREAMED_TRANSCRIPT_LINE_SIZE,
};

// Checking old search text is optional. Bound the decoded work of each verification pass.
pub(super) const LEGACY_PROJECTION_LIMITS: JsonLinesLimits = JsonLinesLimits {
    file_bytes: 64 * 1024 * 1024,
    records: 100_000,
    ..LIMITS
};

pub(super) fn visit_with_limits(
    path: &Path,
    limits: JsonLinesLimits,
    visit: impl FnMut(Value, JsonLineSkips, bool) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records_with_limits(
        open(path)?,
        limits,
        (!compressed(path)).then_some(path),
        visit,
    )
}

pub(super) fn visit(
    path: &Path,
    visit: impl FnMut(Value, JsonLineSkips, bool) -> Result<()>,
) -> Result<JsonLineSkips> {
    visit_with_limits(path, LIMITS, visit)
}

/// Returns whether every nonblank record could be read and the visitor accepted it.
pub(super) fn visit_all_records(
    path: &Path,
    mut visit: impl FnMut(Value, bool) -> Result<bool>,
) -> Result<bool> {
    let mut accepted = true;
    let (omissions, _) = visit_json_reader(
        &mut BufReader::new(open(path)?),
        LIMITS,
        workspace_record_candidate,
        |location, record, _| {
            accepted &= visit(record, location.terminated)?;
            Ok(())
        },
    )?;
    Ok(accepted && omissions.oversized == 0 && omissions.malformed == 0)
}

pub(super) fn visit_all_projected_records(
    path: &Path,
    owner: &str,
    mut accepts: impl FnMut(Value, bool) -> Result<bool>,
) -> Result<bool> {
    let mut accepted = true;
    let mut receipts = super::history_base::replay_messages::RequestReceipts::new(owner, true);
    let mut project = |record, _, confirmed| {
        accepted &= accepts(record, confirmed)?;
        Ok(())
    };
    let omissions = visit(path, |record, skipped, _| {
        receipts.observe_confirmed(record, skipped, &mut project)
    })?;
    receipts.flush(|record, mark| project(record, mark, false))?;
    Ok(accepted && omissions.oversized == 0 && omissions.malformed == 0)
}

pub(super) fn read_records(
    reader: impl Read,
    visit: impl FnMut(Value, JsonLineSkips, bool) -> Result<()>,
) -> Result<JsonLineSkips> {
    read_records_with_limits(reader, LIMITS, None, visit)
}

fn read_records_with_limits(
    reader: impl Read,
    limits: JsonLinesLimits,
    plain: Option<&Path>,
    mut visit: impl FnMut(Value, JsonLineSkips, bool) -> Result<()>,
) -> Result<JsonLineSkips> {
    let session = OnceCell::<Option<String>>::new();
    visit_json_reader_with_locations(
        &mut BufReader::new(reader),
        limits,
        |location, line, oversized| {
            workspace_gap_at(
                plain,
                location,
                line,
                oversized,
                session.get().and_then(Option::as_deref),
            )
        },
        |location, record, skipped_before| {
            session.get_or_init(|| {
                (record["type"] == "session_meta")
                    .then(|| {
                        crate::support::string_at(&record["payload"], &[&["id"], &["session_id"]])
                    })
                    .flatten()
                    .map(str::to_owned)
            });
            visit(record, skipped_before, location.terminated)
        },
    )
    .map(|(omissions, _)| omissions)
}

pub(super) enum Sample {
    Complete(Vec<u8>),
    Edges {
        records: Vec<Value>,
        run_starts: Vec<usize>,
        omitted: usize,
        unreadable: JsonLineSkips,
        prefix_records: usize,
        saved_messages: usize,
        directory: Option<PathBuf>,
    },
    Reconstruct,
}

pub(super) enum WorkspaceRecord {
    SavedMessage,
    Candidate,
    Compacted,
    Other,
}

pub(super) fn workspace_record_candidate(line: &[u8], oversized: bool) -> WorkspaceGap {
    workspace_gap(line, oversized, None)
}

pub(super) fn workspace_gap_at(
    plain: Option<&Path>,
    location: JsonLineLocation,
    line: &[u8],
    oversized: bool,
    session: Option<&str>,
) -> Result<WorkspaceGap> {
    let kind = gap_kind(line, oversized);
    if oversized
        && kind == Some(DamagedWorkspace::Unclassified)
        && let Some(path) = plain
        && !super::oversized_settings_marker(path, &[location])?
    {
        // This row could have changed the turn directory but did not contain saved settings.
        return Ok(WorkspaceGap::TurnOrRollback);
    }
    Ok(classify_gap(kind, line, oversized, session))
}

pub(super) fn workspace_gap(line: &[u8], oversized: bool, session: Option<&str>) -> WorkspaceGap {
    classify_gap(gap_kind(line, oversized), line, oversized, session)
}

fn gap_kind(line: &[u8], oversized: bool) -> Option<DamagedWorkspace> {
    if oversized {
        workspace_damage::oversized_workspace_record(line)
    } else {
        workspace_damage::damaged_workspace_record(line)
    }
}

fn classify_gap(
    kind: Option<DamagedWorkspace>,
    line: &[u8],
    oversized: bool,
    session: Option<&str>,
) -> WorkspaceGap {
    match kind {
        Some(DamagedWorkspace::TurnOrRollback) => WorkspaceGap::TurnOrRollback,
        Some(DamagedWorkspace::PossibleSettings) => {
            let owner =
                field(line, "payload").and_then(|payload| string_field(payload, "thread_id"));
            if session
                .zip(owner.as_deref())
                .is_some_and(|(selected, owner)| !same_thread(selected, owner))
            {
                WorkspaceGap::ForeignSettings
            } else {
                WorkspaceGap::PossibleSettings
            }
        }
        // Later bytes can name saved settings even when the retained prefix ends inside a field.
        Some(DamagedWorkspace::Unclassified) if oversized => WorkspaceGap::PossibleSettings,
        Some(DamagedWorkspace::Unclassified) => WorkspaceGap::Unclassified,
        None => WorkspaceGap::Other,
    }
}

pub(super) fn workspace_record_kind(line: &[u8], inspect_displays: bool) -> WorkspaceRecord {
    if !line.trim_ascii_start().starts_with(b"{") {
        return WorkspaceRecord::Other;
    }
    match string_field(line, "type").as_deref() {
        Some("compacted") => WorkspaceRecord::Compacted,
        Some("response_item" | "event_msg") if inspect_displays => WorkspaceRecord::Candidate,
        None | Some("turn_context" | "session_meta") => WorkspaceRecord::Candidate,
        Some("event_msg") => match field(line, "payload")
            .and_then(|payload| string_field(payload, "type"))
            .as_deref()
        {
            Some("user_message" | "agent_message") => WorkspaceRecord::SavedMessage,
            None | Some("thread_rolled_back" | "thread_settings_applied") => {
                WorkspaceRecord::Candidate
            }
            _ => WorkspaceRecord::Other,
        },
        _ => WorkspaceRecord::Other,
    }
}

fn string_field(line: &[u8], name: &str) -> Option<String> {
    serde_json::Deserializer::from_slice(field(line, name)?)
        .into_iter::<String>()
        .next()?
        .ok()
}

/// Reads object keys in order. A field before the damage still identifies the enclosing record.
fn field<'a>(line: &'a [u8], target: &str) -> Option<&'a [u8]> {
    let mut rest = line.trim_ascii_start().strip_prefix(b"{")?;
    loop {
        let mut key = serde_json::Deserializer::from_slice(rest).into_iter::<String>();
        let name = key.next()?.ok()?;
        rest = rest[key.byte_offset()..]
            .trim_ascii_start()
            .strip_prefix(b":")?;
        if name == target {
            return Some(rest);
        }
        let mut ignored =
            serde_json::Deserializer::from_slice(rest).into_iter::<serde::de::IgnoredAny>();
        ignored.next()?.ok()?;
        rest = rest[ignored.byte_offset()..]
            .trim_ascii_start()
            .strip_prefix(b",")?;
    }
}

/// Read small rollouts once. Larger compressed files have no seekable tail, so sample the
/// decoded stream and inspect every record for a rollback that would invalidate those edges.
pub(super) fn sample(
    path: &Path,
    budget: u64,
    creation_directory: Option<&Path>,
    session: &str,
    history_mode: CodexHistoryMode,
) -> Result<Sample> {
    let mut reader = open(path)?;
    let mut prefix = Vec::new();
    // Indexing normally supplies 16 MiB. Keep a larger caller budget from forcing a huge buffer.
    let budget = budget.min(super::CodexAdapter::FULL_READ_SOURCE_BYTES);
    reader.by_ref().take(budget + 1).read_to_end(&mut prefix)?;
    if u64::try_from(prefix.len())? <= budget {
        return Ok(Sample::Complete(prefix));
    }
    if history_mode != CodexHistoryMode::Legacy {
        return Ok(Sample::Reconstruct);
    }
    let mut head = Vec::new();
    let mut tail = VecDeque::new();
    let mut count = 0;
    let mut prefix_records = 0;
    let mut workspace = WorkspaceScan::new(creation_directory);
    let (unreadable, bytes) = visit_json_reader(
        &mut BufReader::new(Cursor::new(prefix).chain(reader)),
        LIMITS,
        |line, oversized| workspace_gap(line, oversized, Some(session)),
        |loc, record, skipped| {
            let record = super::saved_message::project(record);
            let ordinal = count + skipped.total();
            count += 1;
            workspace.settings.observe_gaps(skipped);
            workspace.observe(session, &record);
            if head.len() < SAMPLE_RECORDS && loc.offset + loc.length <= MAX_PREVIEW_TAIL_SIZE {
                head.push((ordinal, record));
                if ordinal == prefix_records {
                    prefix_records += 1;
                }
            } else {
                while tail.front().is_some_and(|(offset, _, _)| {
                    loc.offset + loc.length - offset > MAX_PREVIEW_TAIL_SIZE
                }) {
                    tail.pop_front();
                }
                if loc.length <= MAX_PREVIEW_TAIL_SIZE {
                    if tail.len() == SAMPLE_RECORDS {
                        tail.pop_front();
                    }
                    tail.push_back((loc.offset, ordinal, record));
                }
            }
            Ok(())
        },
    )?;
    workspace.settings.observe_gaps(unreadable);
    if workspace.reconstruct_history || unreadable.workspace_gaps() > 0 {
        return Ok(Sample::Reconstruct);
    }
    for (offset, ordinal, record) in tail {
        if bytes - offset <= MAX_PREVIEW_TAIL_SIZE {
            if ordinal == prefix_records {
                prefix_records += 1;
            }
            head.push((ordinal, record));
        }
    }
    let run_starts = head
        .windows(2)
        .enumerate()
        .filter_map(|(i, pair)| (pair[1].0 != pair[0].0 + 1).then_some(i + 1))
        .collect();
    Ok(Sample::Edges {
        run_starts,
        saved_messages: workspace.saved_messages,
        omitted: count - head.len(),
        prefix_records,
        unreadable,
        directory: workspace.settings.directory(workspace.directory),
        records: head.into_iter().map(|(_, record)| record).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAdapter;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn a_retained_name_requires_every_original_byte_to_survive_the_append() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let path = fixture.path().join("rollout.jsonl");
        let original = vec![b'a'; 64 * 1024];
        fs::write(&path, &original)?;
        let revision = || CodexAdapter::index_file_revision(&path).unwrap();
        let capture = |bytes| CodexAdapter::rollout_prefix_hash(&path, revision(), bytes);
        let prior = revision();
        let receipt = capture(None).unwrap();
        assert_eq!(receipt.1.as_slice(), &Sha256::digest(&original)[..]);
        let mut file = fs::OpenOptions::new().write(true).open(&path)?;
        file.set_modified(file.metadata()?.modified()? + std::time::Duration::from_secs(2))?;
        assert_eq!(capture(Some(receipt.0)), Some(receipt));
        assert!(capture(Some(receipt.0 + 1)).is_none());
        file.write_all(b"z")?;
        assert_ne!(capture(Some(receipt.0)).unwrap().1, receipt.1);
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"a")?;
        let budget = 16 * 1024 * 1024;
        file.set_len(budget)?;
        let at_limit = capture(None).unwrap();
        assert_eq!(at_limit.0, budget);
        file.set_len(budget + 1)?;
        assert_eq!(capture(Some(budget)), Some(at_limit));
        assert!(capture(None).is_none());
        assert!(CodexAdapter::rollout_prefix_hash(&path, prior, Some(receipt.0)).is_none());
        assert_eq!(capture(Some(receipt.0)), Some(receipt));
        file.seek(SeekFrom::Start(receipt.0 / 2))?;
        file.write_all(b"b")?;
        assert_ne!(capture(Some(receipt.0)).unwrap().1, receipt.1);
        Ok(())
    }

    #[test]
    fn compressed_ancestor_budget_is_shared_across_the_chain() -> Result<()> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("ancestor.jsonl.zst");
        let records = b"fixture data inside and after a bookmark";
        let frame = zstd::bulk::compress(records, 1)?;
        fs::write(&path, &frame)?;
        let size = |n| MAX_STREAMED_TRANSCRIPT_FILE_SIZE - 2 * n + 1;
        let check = |reader: &mut dyn Read| Ok(std::io::copy(reader, &mut std::io::sink())? > 0);
        for mut budget in [
            NativeReadBudget {
                encoded: size(u64::try_from(frame.len())?),
                decoded: 0,
            },
            NativeReadBudget {
                encoded: 0,
                decoded: size(u64::try_from(records.len())?),
            },
        ] {
            assert!(budget.contains_prefix(&path, 1, check)?);
            assert!(!budget.contains_prefix(&path, 1, check)?);
        }
        let plain = home.path().join("ancestor.jsonl");
        fs::write(&plain, records)?;
        let mut budget = NativeReadBudget {
            decoded: MAX_STREAMED_TRANSCRIPT_FILE_SIZE - u64::try_from(records.len())?,
            ..NativeReadBudget::default()
        };
        assert!(budget.contains_prefix(&plain, 1, check)?);
        assert!(!budget.contains_prefix(&path, 1, check)?);
        Ok(())
    }
}
