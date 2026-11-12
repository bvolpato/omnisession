use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use omnis_core::{SearchDocumentBuilder, SearchTruncationStrategy, session_preview};
use omnis_ir::{
    CanonicalSnapshot, EventKind, OmniEvent, Provider, ReplayPolicy, SessionRef, SourceRevision,
};
use prost::Message;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::{
    IncompleteTranscript, IndexRead, IndexedSearchDocument, LaunchPlan, LaunchTarget,
    NativeSession, PriorSearchDocument, ProviderAdapter, ProviderInstallation,
    support::{
        EventBuilder, JsonLineDamage, JsonLineTail, MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
        SqliteSnapshotCache, estimated_event_bytes, json_lines_preview_with_prefix,
        parse_timestamp, paths_match, provider_executable, provider_file, provider_root,
        sort_sessions, sqlite_snapshot, store_is_missing, validate_provider,
        visit_json_lines_with_tail, visit_limited_search_edges, visit_streamed_json_file,
    },
};

const SUMMARY_DATABASE: &str = "conversation_summaries.db";
const PREVIEW_RECORDS: usize = 1_024;

#[derive(Clone, Debug)]
pub struct AntigravityAdapter {
    root: Option<PathBuf>,
    summaries: Arc<SqliteSnapshotCache>,
}

impl AntigravityAdapter {
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            summaries: Arc::default(),
        }
    }

    fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .ok_or_else(|| anyhow!("Antigravity data root was not found"))
    }

    fn summary(&self, id: &str) -> Result<Summary> {
        validate_id(id)?;
        let root = self.root()?;
        let database = root.join(SUMMARY_DATABASE);
        self.summaries
            .read(root, &database, |connection| summary_by_id(connection, id))
    }

    fn source(&self, summary: &Summary) -> Option<PathBuf> {
        let root = self.root.as_deref()?;
        validate_id(&summary.id).ok()?;
        selected_history(root, &history_paths(root, &summary.id))
    }

    fn open_history(
        &self,
        summary: &Summary,
        path: &Path,
    ) -> Result<(
        std::fs::File,
        u64,
        NativeSession,
        crate::source_file::ObservedSourceFile,
    )> {
        anyhow::ensure!(
            self.source(summary).as_deref() == Some(path),
            "Antigravity history changed during the search read"
        );
        let file = std::fs::File::open(path)
            .context("Antigravity history changed during the search read")?;
        let before = file.metadata()?;
        let native = summary.native_session(Some(path.to_owned()));
        let observed = crate::source_file::ObservedSourceFile::from_open_file(path, &file, &before)
            .context("Antigravity history changed during the search read")?;
        anyhow::ensure!(
            native.source_revision == cli_history_revision(path, observed, summary.step_count),
            "Antigravity history changed during the search read"
        );
        Ok((file, before.len(), native, observed))
    }

    fn ensure_history_unchanged(
        &self,
        summary: &Summary,
        path: &Path,
        observed: crate::source_file::ObservedSourceFile,
        handle: &std::fs::File,
    ) -> Result<()> {
        observed.ensure_unchanged(path, path, handle)?;
        anyhow::ensure!(
            self.source(summary).as_deref() == Some(path),
            "Antigravity history changed during the search read"
        );
        Ok(())
    }

    fn read_observed(
        &self,
        session: &SessionRef,
        path: Option<&Path>,
        preview: bool,
        tail: JsonLineTail<'_>,
    ) -> Result<IndexRead> {
        validate_provider(session, Provider::Antigravity)?;
        let summary = self.summary(&session.id)?;
        let selected = self.source(&summary);
        anyhow::ensure!(
            path.is_none_or(|path| selected.as_deref() == Some(path)),
            "Antigravity history changed during the search read"
        );
        let native = summary.native_session(selected.clone());
        anyhow::ensure!(
            selected.is_none() || native.source_revision.is_some(),
            "Antigravity history changed during the search read"
        );
        let read = self.read_summary(session, &summary, preview, tail);
        let current = self.summary(&session.id)?;
        anyhow::ensure!(
            self.source(&current) == selected
                && current == summary
                && current.native_session(selected).source_revision == native.source_revision,
            "Antigravity history changed during the search read"
        );
        let history = read?;
        Ok(IndexRead {
            workspace_projection: crate::CodexWorkspaceProjection::Unchecked,
            rollback_title: crate::RollbackTitle::Unknown,
            verified_prefix_end: history.verified_prefix_end.or_else(|| {
                tail.damage()
                    .filter(|_| !preview)
                    .and_then(JsonLineDamage::verified_prefix_end)
            }),
            database_aliases: history.database_aliases.into(),
            search_record_groups: history.search_record_groups,
            snapshot: history.snapshot,
            source_complete: !preview
                && history.omitted_database_rows == 0
                && tail.damage().is_none_or(|damage| damage.count() == 0),
            provider_limited: !preview && history.omitted_database_rows > 0,
            source_revision: native.source_revision.unwrap_or_default(),
            native_session: Some(native),
        })
    }

    fn read(&self, session: &SessionRef, preview: bool) -> Result<CanonicalSnapshot> {
        validate_provider(session, Provider::Antigravity)?;
        let summary = self.summary(&session.id)?;
        let torn_tail = JsonLineDamage::default();
        self.read_summary(
            session,
            &summary,
            preview,
            JsonLineTail::ReportIncomplete(&torn_tail),
        )
        .map(|history| history.snapshot)
    }

    fn read_summary(
        &self,
        session: &SessionRef,
        summary: &Summary,
        preview: bool,
        tail: JsonLineTail<'_>,
    ) -> Result<ObservedHistory> {
        let mut builder = EventBuilder::new(Provider::Antigravity, &session.id);

        let mut groups = Vec::new();
        let mut verified_prefix_end = None;
        let mut omitted_database_rows = 0;
        let mut database_aliases = omnis_ir::antigravity_receipts::DatabaseAliases::new();
        match self.source(summary) {
            Some(path) if path.extension().is_some_and(|ext| ext == "jsonl") => {
                let mut push = |record: Value| {
                    let start = builder.checkpoint();
                    push_transcript_record(&mut builder, &record);
                    let end = builder.checkpoint();
                    if !preview && let Some(damage) = tail.damage() {
                        damage.confirm_prefix(end);
                    }
                    if end - start > 1 {
                        groups.push(start..end);
                    }
                    start
                };
                let oversized = if preview {
                    let (records, verified) =
                        json_lines_preview_with_prefix(&path, PREVIEW_RECORDS, tail)?;
                    for (index, record) in records.into_iter().enumerate() {
                        let start = push(record);
                        if index == verified {
                            verified_prefix_end = Some(start);
                        }
                    }
                    0
                } else {
                    visit_json_lines_with_tail(
                        &path,
                        MAX_STREAMED_TRANSCRIPT_FILE_SIZE,
                        tail,
                        |record| {
                            push(record);
                            Ok(())
                        },
                    )?
                };
                builder.push_oversized_record_notice(oversized, summary.updated_at);
                verify_jsonl_history(summary, &builder)?;
                record_jsonl_damage(&mut builder, tail, summary.updated_at);
            }
            Some(path) => {
                let (_, coverage) =
                    push_database_steps(self.root()?, &path, &mut builder, summary.step_count)?;
                verified_prefix_end = coverage.verified_prefix_end;
                omitted_database_rows = coverage.omitted;
                database_aliases = coverage.aliases;
            }
            None if summary.step_count > 0 => {
                return Err(anyhow!(
                    "Antigravity session `{}` declares history but has no readable transcript",
                    session.id
                ));
            }
            None => {}
        }

        Ok(ObservedHistory {
            snapshot: summary.snapshot(session, builder),
            database_aliases,
            search_record_groups: groups,
            verified_prefix_end,
            omitted_database_rows,
        })
    }
}

struct ObservedHistory {
    database_aliases: omnis_ir::antigravity_receipts::DatabaseAliases,
    omitted_database_rows: usize,
    snapshot: CanonicalSnapshot,
    search_record_groups: Vec<std::ops::Range<u64>>,
    verified_prefix_end: Option<u64>,
}

#[derive(Default)]
struct DamagedRecordPositions {
    dated_records: Vec<(usize, usize, i64)>,
    interior_seen: usize,
    starts: Vec<usize>,
}

impl DamagedRecordPositions {
    fn observe_suffix(&mut self, record: &serde_json::Value, before: usize, after: usize) {
        if self.interior_seen > 0
            && after > before
            && after <= 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT
            && let Some(time) =
                parse_timestamp(record.get("created_at")).and_then(|t| t.timestamp_nanos_opt())
        {
            let start = if before == 0 { 0 } else { before + 2 };
            self.dated_records.push((start, after, time));
        }
    }

    fn observe(&mut self, damage: &JsonLineDamage, document: &SearchDocumentBuilder) -> usize {
        let bytes = document.projected_byte_count();
        if damage.interior_count() > self.interior_seen || damage.final_record() {
            self.interior_seen = damage.interior_count();
            if bytes <= 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT
                && self.starts.last() != Some(&bytes)
            {
                self.starts.push(bytes);
            }
        }
        bytes
    }
}

// A growing partial transcript can publish more confirmed turns if its earlier search text still matches.
struct PreviousSearchPrefix {
    expected: Option<(usize, [u8; 32])>,
    verified: bool,
}

impl PreviousSearchPrefix {
    fn new(prior: Option<PriorSearchDocument>) -> Self {
        let expected = match prior {
            Some(
                PriorSearchDocument::Complete {
                    byte_count: n,
                    indexed_hash,
                }
                | PriorSearchDocument::Partial {
                    byte_count: n @ 1..,
                    indexed_hash,
                    ..
                },
            ) => Some((n, indexed_hash)),
            _ => None,
        };
        if let Some((0, hash)) = expected {
            return Self {
                expected: None,
                verified: sha2::Sha256::digest(b"").as_slice() == hash,
            };
        }
        Self {
            expected,
            verified: expected.is_none(),
        }
    }

    fn observe(&mut self, document: &SearchDocumentBuilder) {
        if let Some((length, hash)) = self.expected
            && document.source_byte_count() >= length
        {
            self.verified =
                document.source_byte_count() == length && document.indexed_text_hash() == hash;
            self.expected = None;
        }
    }

    fn sample_at(
        file: &mut std::fs::File,
        session: &SessionRef,
        end: u64,
    ) -> Result<SearchDocumentBuilder> {
        let damage = JsonLineDamage::default();
        let (records, _) = crate::support::json_lines_preview_at(
            file,
            PREVIEW_RECORDS,
            JsonLineTail::ReportIncomplete(&damage),
            end,
        )?;
        let mut builder = EventBuilder::new(Provider::Antigravity, &session.id);
        let mut document = SearchDocumentBuilder::default();
        for record in records {
            push_transcript_record(&mut builder, &record);
            for event in builder.drain_events() {
                document.push(&event);
            }
        }
        Ok(document)
    }

    fn verified_preview(
        &mut self,
        preview: IndexRead,
        session: &SessionRef,
        mut file: std::fs::File,
        length: u64,
        prior: Option<PriorSearchDocument>,
        torn: bool,
    ) -> Result<IndexedSearchDocument> {
        self.observe_preview(&preview.snapshot, torn);
        self.verify(torn, &mut file, session, length, prior, false)?;
        let mut indexed: IndexedSearchDocument = preview.into();
        if let Some(projection) = &mut indexed.document.projection {
            projection.sampled_file_bytes = Some(length);
        }
        Ok(indexed)
    }

    fn observe_preview(&mut self, snapshot: &CanonicalSnapshot, torn: bool) {
        if torn && self.expected.is_some() {
            let mut document = SearchDocumentBuilder::default();
            for event in &snapshot.events {
                document.push(event);
                self.observe(&document);
                if self.expected.is_none() {
                    break;
                }
            }
        }
    }

    fn required_edge_bytes(
        full_read_bytes: u64,
        prior: Option<PriorSearchDocument>,
    ) -> Option<usize> {
        match (full_read_bytes, prior) {
            (
                u64::MAX,
                Some(
                    PriorSearchDocument::RequireCompleteTail(bytes)
                    | PriorSearchDocument::Complete {
                        byte_count: bytes, ..
                    },
                ),
            ) => Some(bytes),
            _ => None,
        }
    }

    fn tail_policy(
        damage: &JsonLineDamage,
        prior: Option<PriorSearchDocument>,
        limited: bool,
    ) -> JsonLineTail<'_> {
        if matches!(prior, Some(PriorSearchDocument::RequireCompleteTail(_)))
            || (limited && matches!(prior, Some(PriorSearchDocument::Complete { .. })))
        {
            JsonLineTail::RequireCompleteTail(damage)
        } else {
            JsonLineTail::ReportIncomplete(damage)
        }
    }

    fn needs_damage_check(damage: &JsonLineDamage, prior: Option<PriorSearchDocument>) -> bool {
        if matches!(prior, Some(PriorSearchDocument::Complete { .. }))
            && damage.interior_count() > 0
        {
            return false;
        }
        damage.final_record()
            || (damage.interior_count() > 0
                && matches!(
                    prior,
                    Some(PriorSearchDocument::Partial {
                        recover_from_full_scan: true,
                        sampled_file_bytes: None,
                        ..
                    })
                ))
    }

    fn verify(
        &self,
        torn: bool,
        file: &mut std::fs::File,
        session: &SessionRef,
        length: u64,
        prior: Option<PriorSearchDocument>,
        full_scan: bool,
    ) -> Result<Option<([u8; 32], usize)>> {
        if !torn || self.verified {
            return Ok(None);
        }
        if let Some(PriorSearchDocument::Partial {
            byte_count,
            indexed_hash,
            sampled_file_bytes,
            recover_from_full_scan,
        }) = prior
        {
            if let Some(old) = sampled_file_bytes
                && old <= length
            {
                let sample = Self::sample_at(file, session, old)?;
                if sample.source_byte_count() == byte_count
                    && sample.indexed_text_hash() == indexed_hash
                {
                    return Ok(None);
                }
            } else if full_scan && recover_from_full_scan {
                let sample = Self::sample_at(file, session, length)?.finish();
                return Ok(Some(sample.projection.map_or_else(
                    || (sha2::Sha256::digest(b"").into(), 0),
                    |p| (p.digest, p.byte_count),
                )));
            }
        }
        Err(IncompleteTranscript.into())
    }
}

fn record_jsonl_damage(
    builder: &mut EventBuilder,
    tail: JsonLineTail<'_>,
    timestamp: Option<DateTime<Utc>>,
) {
    if let Some(damage) = tail.damage().filter(|damage| damage.count() > 0) {
        builder.push(
            EventKind::ProviderEvent,
            json!({"omitted_events": damage.count(), "native_history_unverified": true, "incomplete_final_record": damage.final_record()}),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some(if damage.final_record() { "omnisession.antigravity_incomplete_tail" } else { "omnisession.antigravity_malformed_record" }.to_owned()),
            None,
        );
    }
}

fn finish_search_document(
    document: SearchDocumentBuilder,
    gaps: DamagedRecordPositions,
    length: u64,
    provider_limited: bool,
    torn_final: bool,
    legacy_sample: Option<([u8; 32], usize)>,
    verified_append: bool,
) -> omnis_core::SearchDocument {
    let mut document = document.finish();
    if length <= MAX_STREAMED_TRANSCRIPT_FILE_SIZE
        && let Some(projection) = document.projection.as_mut()
    {
        projection.fully_scanned_bytes = Some(length);
        projection.torn_final = torn_final;
        projection.verified_append = verified_append;
        projection.legacy_damage_sample = legacy_sample;
        if gaps.interior_seen > 0 {
            projection.dated_suffix_records = gaps.dated_records;
            projection.interior_damage = Some(gaps.starts);
        }
    }
    if provider_limited {
        document.source_complete = false;
        document.truncated = true;
        document.truncation_strategy = SearchTruncationStrategy::ProviderReadLimit;
    }
    document
}

fn project_search_record(
    record: &Value,
    events: &mut EventBuilder,
    document: &mut SearchDocumentBuilder,
    title_source: &mut CanonicalSnapshot,
) {
    let start = events.checkpoint();
    push_transcript_record(events, record);
    for event in events.drain_events() {
        document.push_record_event(&event, start);
        capture_first_message(title_source, event);
    }
}

fn capture_first_message(snapshot: &mut CanonicalSnapshot, event: OmniEvent) {
    if snapshot.events.is_empty()
        && matches!(
            event.kind,
            EventKind::MessageUser | EventKind::MessageAssistant
        )
    {
        snapshot.events.push(event);
        if session_preview(snapshot).first.is_none() {
            snapshot.events.clear();
        }
    }
}

fn verify_jsonl_history(summary: &Summary, events: &EventBuilder) -> Result<()> {
    if summary.step_count > 0 && events.checkpoint() == 0 {
        return Err(IncompleteTranscript).with_context(|| format!(
            "Antigravity session `{}` declares history but its transcript contains no readable steps", summary.id
        ));
    }
    Ok(())
}

fn selected_history(root: &Path, paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find_map(|path| provider_file(root, path))
}

fn history_paths(root: &Path, id: &str) -> [PathBuf; 3] {
    let logs = root.join("brain").join(id).join(".system_generated/logs");
    [
        logs.join("transcript_full.jsonl"),
        logs.join("transcript.jsonl"),
        root.join("conversations").join(format!("{id}.db")),
    ]
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        Self {
            root: provider_root("ANTIGRAVITY_CLI_HOME", &[".gemini", "antigravity-cli"]),
            summaries: Arc::default(),
        }
    }
}

impl ProviderAdapter for AntigravityAdapter {
    fn provider(&self) -> Provider {
        Provider::Antigravity
    }

    fn discovery_cache_home(&self) -> std::io::Result<Option<PathBuf>> {
        crate::support::discovery_home(self.root.as_deref())
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(Provider::Antigravity);
        let data_root = self.root.clone().filter(|root| root.is_dir());
        ProviderInstallation {
            provider: Provider::Antigravity,
            installed: executable.is_some() || data_root.is_some(),
            executable,
            data_root,
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Vec::new());
        };
        let database = root.join(SUMMARY_DATABASE);
        if store_is_missing(&database) {
            return Ok(Vec::new());
        }
        let snapshot = sqlite_snapshot(root, &database)
            .context("failed to snapshot Antigravity summary database")?;
        let mut statement = snapshot.connection.prepare(
            "SELECT conversation_id, title, step_count, last_modified_time, workspace_uris \
             FROM conversation_summaries",
        )?;
        let rows = statement.query_map([], summary_from_row)?;
        let mut sessions = Vec::new();
        for row in rows {
            let summary = row?;
            if validate_id(&summary.id).is_err()
                || project.is_some_and(|requested| {
                    summary
                        .project_path
                        .as_deref()
                        .is_none_or(|recorded| !paths_match(recorded, requested))
                })
            {
                continue;
            }
            let source_path = self.source(&summary);
            sessions.push(summary.native_session(source_path));
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    fn selected_session_at(&self, id: &str, path: &Path) -> Result<Option<NativeSession>> {
        let summary = self.summary(id)?;
        Ok(self
            .source(&summary)
            .filter(|selected| selected == path)
            .map(|selected| summary.native_session(Some(selected))))
    }

    fn read_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, false)
    }

    fn preview_session(&self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        self.read(session, true)
    }

    fn read_full_index_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<IndexRead> {
        self.read_observed(
            session,
            source_path,
            false,
            JsonLineTail::ReportIncomplete(&JsonLineDamage::default()),
        )
    }

    fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        let preview = full_read_bytes == 0
            || source_path
                .and_then(|path| path.metadata().ok())
                .is_some_and(|metadata| metadata.len() > full_read_bytes);
        if preview {
            self.read_observed(
                session,
                source_path,
                true,
                JsonLineTail::ReportIncomplete(&JsonLineDamage::default()),
            )
        } else {
            self.read_observed(session, source_path, false, JsonLineTail::RequireComplete)
        }
    }

    fn index_search_document(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
        prior: Option<PriorSearchDocument>,
    ) -> Result<IndexedSearchDocument> {
        let Some(path) =
            source_path.filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        else {
            return self
                .index_session(session, source_path, full_read_bytes)
                .map(Into::into);
        };
        validate_provider(session, Provider::Antigravity)?;
        let summary = self.summary(&session.id)?;
        let (file, length, native, observed) = self.open_history(&summary, path)?;
        let mut handle = file.try_clone()?;
        let torn = JsonLineDamage::default();
        let limited = length > MAX_STREAMED_TRANSCRIPT_FILE_SIZE;
        let tail = PreviousSearchPrefix::tail_policy(&torn, prior, limited);
        let mut prefix = PreviousSearchPrefix::new(prior);
        if !limited && length > full_read_bytes {
            let preview = self.read_observed(session, Some(path), true, tail)?;
            let result =
                prefix.verified_preview(preview, session, file, length, prior, torn.final_record());
            self.ensure_history_unchanged(&summary, path, observed, &handle)?;
            return result;
        }
        let mut events = EventBuilder::new(Provider::Antigravity, &session.id);
        let mut title_source = summary.snapshot(
            session,
            EventBuilder::new(Provider::Antigravity, &session.id),
        );
        let mut document = SearchDocumentBuilder::default();
        let mut gaps = DamagedRecordPositions::default();
        let mut push = |record| {
            let before = gaps.observe(&torn, &document);
            project_search_record(&record, &mut events, &mut document, &mut title_source);
            torn.confirm_prefix(events.checkpoint());
            gaps.observe_suffix(&record, before, document.projected_byte_count());
            prefix.observe(&document);
            document.source_byte_count()
        };
        let prior_prefix = PreviousSearchPrefix::required_edge_bytes(full_read_bytes, prior);
        let oversized = if limited {
            visit_limited_search_edges(file, prior_prefix, PREVIEW_RECORDS, tail, &mut push)
                .map(|()| 0)
        } else {
            visit_streamed_json_file(file, MAX_STREAMED_TRANSCRIPT_FILE_SIZE, tail, |record| {
                push(record);
                Ok(())
            })
        };
        let checked = oversized.and_then(|count| {
            prefix
                .verify(
                    PreviousSearchPrefix::needs_damage_check(&torn, prior),
                    &mut handle,
                    session,
                    length,
                    prior,
                    !limited,
                )
                .map(|sample| (count, sample))
        });
        self.ensure_history_unchanged(&summary, path, observed, &handle)?;
        let (oversized, legacy_sample) = checked?;
        let verified_append = prefix.verified
            && matches!(prior, Some(PriorSearchDocument::Complete { byte_count, .. }) if document.source_byte_count() > byte_count);
        events.push_oversized_record_notice(oversized, summary.updated_at);
        verify_jsonl_history(&summary, &events)?;
        record_jsonl_damage(&mut events, tail, summary.updated_at);
        for event in events.drain_events() {
            document.push(&event);
        }
        gaps.observe(&torn, &document);
        let document = finish_search_document(
            document,
            gaps,
            length,
            limited && prior_prefix.is_some(),
            torn.final_record(),
            legacy_sample,
            verified_append,
        );
        Ok(IndexedSearchDocument {
            document,
            read: IndexRead {
                workspace_projection: crate::CodexWorkspaceProjection::Unchecked,
                rollback_title: crate::RollbackTitle::Unknown,
                verified_prefix_end: torn.verified_prefix_end(),
                database_aliases: std::sync::Arc::default(),
                search_record_groups: Vec::new(),
                snapshot: title_source,
                source_complete: !limited && torn.count() == 0,
                provider_limited: limited && prior_prefix.is_some(),
                source_revision: native
                    .source_revision
                    .context("Antigravity history changed during the search read")?,
                native_session: Some(native),
            },
        })
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        let mut args = Vec::new();
        if let Some(prompt) = &target.prompt {
            args.push("--prompt-interactive".to_owned());
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "agy".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, Provider::Antigravity)?;
        validate_id(&session.id)?;
        if target.fork {
            return Err(anyhow!(
                "agy 1.1.8 has no documented native conversation fork command"
            ));
        }
        let mut args = vec!["--conversation".to_owned(), session.id.clone()];
        if let Some(prompt) = &target.prompt {
            args.push("--prompt-interactive".to_owned());
            args.push(prompt.clone());
        }
        Ok(LaunchPlan {
            program: "agy".to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[derive(Debug, PartialEq)]
struct Summary {
    id: String,
    title: String,
    step_count: usize,
    updated_at: Option<DateTime<Utc>>,
    project_path: Option<PathBuf>,
}

impl Summary {
    fn snapshot(&self, session: &SessionRef, events: EventBuilder) -> CanonicalSnapshot {
        events.snapshot(
            session.clone(),
            nonempty(self.title.clone()),
            self.project_path.clone(),
            None,
            self.updated_at.unwrap_or_else(Utc::now),
        )
    }

    fn native_session(&self, source_path: Option<PathBuf>) -> NativeSession {
        let session = SessionRef::new(Provider::Antigravity, self.id.clone());
        let revision = source_path.as_deref().and_then(|path| {
            cli_history_revision(
                path,
                crate::source_file::ObservedSourceFile::read(path)?,
                self.step_count,
            )
        });
        let updated_at = self.updated_at.or_else(|| {
            source_path
                .as_ref()?
                .metadata()
                .ok()?
                .modified()
                .ok()
                .map(Into::into)
        });
        NativeSession {
            session,
            title: nonempty(self.title.clone()),
            project_path: self.project_path.clone(),
            git_branch: None,
            created_at: None,
            updated_at,
            updated_at_approximate: false,
            event_count: self.step_count,
            source_path,
            source_revision: revision,
        }
    }
}

#[derive(serde::Serialize)]
enum WalRevision {
    Missing,
    Present(Uuid),
}

fn cli_history_revision(
    path: &Path,
    observed: crate::source_file::ObservedSourceFile,
    declared_steps: usize,
) -> Option<SourceRevision> {
    let mut revision = observed.revision;
    if matches!(
        path.file_name().and_then(|s| s.to_str()),
        Some("transcript.jsonl" | "transcript_full.jsonl")
    ) {
        // Both filenames can name the same file after a rename. Older readers used the selected name.
        revision = observed.revision_in_namespace(&path.with_file_name("transcript_full.jsonl"));
        let fallback = observed
            .revision_in_namespace(&path.with_file_name("transcript.jsonl"))
            .file;
        revision.legacy_file = fallback.filter(|file| Some(*file) != revision.file);
    }
    history_revision(path, revision, declared_steps)
}

pub(super) fn history_revision(
    path: &Path,
    revision: SourceRevision,
    declared_steps: usize,
) -> Option<SourceRevision> {
    let file = revision.file?;
    let history = revision.contents?;
    let database = path.extension().is_some_and(|ext| ext == "db");
    let wal = if database {
        let sidecar = crate::support::sidecar(path, "-wal");
        match sidecar.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {
                let wal = provider_file(path.parent()?, &sidecar)?;
                Some(WalRevision::Present(
                    crate::source_file_revision(&wal)?.contents?,
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some(WalRevision::Missing)
            }
            _ => return None,
        }
    } else {
        None
    };
    let signature = serde_json::to_vec(&(
        history,
        wal,
        database.then_some(declared_steps),
        None::<bool>,
    ))
    .ok()?;
    Some(SourceRevision {
        contents: Some(Uuid::new_v5(&file, &signature)),
        ..revision
    })
}

fn summary_by_id(connection: &rusqlite::Connection, id: &str) -> Result<Summary> {
    connection
        .query_row(
            "SELECT conversation_id, title, step_count, last_modified_time, \
             workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
            [id],
            summary_from_row,
        )
        .optional()?
        .ok_or_else(|| anyhow!("Antigravity session `{id}` was not found"))
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Summary> {
    let workspace_uris: String = row.get(4)?;
    let step_count = row.get::<_, i64>(2)?;
    Ok(Summary {
        id: row.get(0)?,
        title: row.get(1)?,
        step_count: usize::try_from(step_count.max(0)).unwrap_or(usize::MAX),
        updated_at: row
            .get::<_, String>(3)
            .ok()
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc)),
        project_path: workspace_path(&workspace_uris),
    })
}

fn workspace_path(serialized: &str) -> Option<PathBuf> {
    let uri = serde_json::from_str::<Vec<String>>(serialized)
        .ok()?
        .into_iter()
        .next()?;
    workspace_uri_path(&uri)
}

/// Decodes a local `file://` workspace URI.
pub(crate) fn workspace_uri_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    #[cfg(windows)]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|path| path.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(&decoded);
    Some(PathBuf::from(decoded))
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| anyhow!("invalid Antigravity conversation ID `{id}`"))
}

pub(crate) fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn push_transcript_record(builder: &mut EventBuilder, record: &Value) {
    let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = parse_timestamp(record.get("created_at"));
    let status = record.get("status").and_then(Value::as_str).unwrap_or("");
    match record_type {
        "USER_INPUT" => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
                let text = antigravity_user_request(text);
                if text.is_empty() {
                    return;
                }
                builder.push(
                    EventKind::MessageUser,
                    json!({"text": text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(record_type.to_owned()),
                    None,
                );
            }
        }
        "PLANNER_RESPONSE" => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({"text": text}),
                    timestamp,
                    ReplayPolicy::Contextual,
                    Some(record_type.to_owned()),
                    None,
                );
            }
            if let Some(calls) = record.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    builder.push(
                        EventKind::ToolCalled,
                        json!({"name": name, "arguments": call.get("args").cloned().unwrap_or(Value::Null)}),
                        timestamp,
                        ReplayPolicy::HistoricalOnly,
                        Some(record_type.to_owned()),
                        None,
                    );
                }
            }
        }
        "RUN_COMMAND" => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::CommandExecuted,
        ),
        "CHECKPOINT" => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::CheckpointCreated,
        ),
        "CODE_ACTION" | "WRITE_TO_FILE" | "FILE_CHANGE" => {
            push_historical(
                builder,
                record_type,
                status,
                timestamp,
                EventKind::FilePatch,
            );
        }
        "VIEW_FILE" | "READ_URL_CONTENT" | "READ_RESOURCE" => {
            push_historical(builder, record_type, status, timestamp, EventKind::FileRead);
        }
        "" => {}
        _ => push_historical(
            builder,
            record_type,
            status,
            timestamp,
            EventKind::ProviderEvent,
        ),
    }
}

pub(crate) fn antigravity_user_request(text: &str) -> &str {
    let Some(request) = text.strip_prefix("<USER_REQUEST>") else {
        return text;
    };
    request
        .split_once("</USER_REQUEST>")
        .map_or(text, |(request, _)| request.trim())
}

fn push_historical(
    builder: &mut EventBuilder,
    record_type: &str,
    status: &str,
    timestamp: Option<DateTime<Utc>>,
    kind: EventKind,
) {
    builder.push(
        kind,
        json!({"status": status}),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        Some(record_type.to_owned()),
        None,
    );
}

/// Returns the decoded step count and the exclusive event sequence at the earliest gap.
pub(crate) struct DatabaseCoverage {
    pub(crate) aliases: omnis_ir::antigravity_receipts::DatabaseAliases,
    pub(crate) verified_prefix_end: Option<u64>,
    pub(crate) omitted: usize,
}

impl DatabaseCoverage {
    fn report(
        &self,
        builder: &mut EventBuilder,
        unsupported: usize,
        order_verified: bool,
        declared_steps: usize,
        stored: usize,
    ) {
        if self.omitted > 0 || unsupported > 0 || !order_verified {
            let mut notice =
                json!({"omitted_events": self.omitted, "unsupported_text_records": unsupported});
            if !order_verified {
                notice["native_history_unverified"] = json!(true);
            }
            if declared_steps > stored {
                notice["declared_steps"] = json!(declared_steps);
                notice["stored_steps"] = json!(stored);
            }
            builder.push(
                EventKind::ProviderEvent,
                notice,
                None,
                ReplayPolicy::HistoricalOnly,
                Some("omnisession.antigravity_step_limit".to_owned()),
                None,
            );
        }
    }
}

pub(crate) fn push_database_steps(
    root: &Path,
    database: &Path,
    builder: &mut EventBuilder,
    declared_steps: usize,
) -> Result<(usize, DatabaseCoverage)> {
    push_database_steps_with_limits(
        root,
        database,
        builder,
        declared_steps,
        DatabaseReadLimits::default(),
    )
}

#[derive(Clone, Copy)]
struct DatabaseReadLimits {
    row_bytes: usize,
    wire_bytes: usize,
    history_bytes: usize,
    records: usize,
}

impl Default for DatabaseReadLimits {
    fn default() -> Self {
        Self {
            row_bytes: 16 * 1024 * 1024,
            wire_bytes: 64 * 1024 * 1024,
            history_bytes: 64 * 1024 * 1024,
            records: omnis_ir::ANTIGRAVITY_DATABASE_STEP_LIMIT,
        }
    }
}

fn push_database_steps_with_limits(
    root: &Path,
    database: &Path,
    builder: &mut EventBuilder,
    declared_steps: usize,
    limits: DatabaseReadLimits,
) -> Result<(usize, DatabaseCoverage)> {
    let snapshot = sqlite_snapshot(root, database)
        .context("failed to snapshot Antigravity conversation database")?;
    let stored = usize::try_from(snapshot.connection.query_row(
        "SELECT count(*) FROM steps",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    if stored == 0 && declared_steps > 0 {
        return Err(anyhow!(
            "Antigravity summary declares {declared_steps} steps but its conversation database has no readable steps"
        ));
    }
    let (key, newest_first) = database_step_order(&snapshot.connection)?;
    // Look at sizes first so a skipped step does not read its blob or displace a smaller one.
    let mut statement = snapshot.connection.prepare(&format!(
        "SELECT {key}, CASE WHEN typeof(step_payload) = 'blob' THEN length(step_payload) END, \
         CASE WHEN typeof(idx) = 'integer' THEN idx END \
         FROM steps ORDER BY {newest_first} LIMIT ?1"
    ))?;
    let mut payload_by_key = snapshot
        .connection
        .prepare(&format!("SELECT step_payload FROM steps WHERE {key} = ?1"))?;
    let mut rows = statement.query([i64::try_from(limits.records)?])?;
    let mut aliases = omnis_ir::antigravity_receipts::DatabaseAliases::new();
    let mut retained = Vec::new();
    let mut position = 0;
    let mut decoded = 0;
    let mut remaining_wire = limits.wire_bytes;
    let mut remaining_history = limits.history_bytes;
    let mut unsupported = 0;
    let mut damaged = 0;
    let mut previous_index = None;
    let mut oldest_ambiguous_row = Some(0);
    while let Some(row) = rows.next()? {
        position += 1;
        let index: Option<i64> = row.get(2)?;
        if index.is_none() {
            oldest_ambiguous_row = None;
        } else if index == previous_index && oldest_ambiguous_row.is_some() {
            oldest_ambiguous_row = Some(position);
        }
        previous_index = index;
        let Some(length) = row.get::<_, Option<i64>>(1)?.filter(|length| *length > 0) else {
            damaged += 1;
            continue;
        };
        let bytes = usize::try_from(length)?;
        if bytes > limits.row_bytes || bytes > remaining_wire {
            continue;
        }
        remaining_wire -= bytes;
        let row_key = rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(0)?);
        let payload: Vec<u8> = payload_by_key.query_row([row_key], |record| record.get(0))?;
        let Ok(step) = ProtoStep::decode(payload.as_slice()) else {
            damaged += 1;
            continue;
        };
        decoded += 1;
        let mut event = database_step_event(step, &payload);
        unsupported += usize::from(event.replay_policy == ReplayPolicy::HistoricalOnly);
        let bytes = estimated_event_bytes(&event.payload);
        if bytes <= remaining_history {
            remaining_history -= bytes;
            if let Some(index) = index {
                record_database_proof(&mut aliases, builder, &mut event, index, &payload);
            }
            retained.push((position, index, event));
        }
    }
    if decoded == 0 && damaged > 0 {
        return Err(anyhow!(
            "no Antigravity steps could be decoded ({damaged} selected step payloads were missing or invalid)"
        ));
    }
    let omitted = stored.max(declared_steps) - retained.len();
    let prefix_end = push_ordered_steps(
        builder,
        retained,
        stored,
        declared_steps != stored && omitted > 0,
        oldest_ambiguous_row,
    );
    let coverage = DatabaseCoverage {
        aliases,
        verified_prefix_end: prefix_end,
        omitted,
    };
    coverage.report(
        builder,
        unsupported,
        oldest_ambiguous_row == Some(0),
        declared_steps,
        stored,
    );
    Ok((decoded, coverage))
}

fn push_ordered_steps(
    builder: &mut EventBuilder,
    retained: Vec<(usize, Option<i64>, DatabaseStepEvent)>,
    stored: usize,
    require_contiguous_prefix: bool,
    oldest_ambiguous_row: Option<usize>,
) -> Option<u64> {
    let mut prefix_end = None;
    let mut position = stored;
    for (row, index, event) in retained.into_iter().rev() {
        let missing_before = require_contiguous_prefix
            && index.and_then(|idx| usize::try_from(idx).ok()) != Some(stored - row);
        if missing_before
            || row != position
            || oldest_ambiguous_row.is_none_or(|oldest| row <= oldest)
            || event.replay_policy == ReplayPolicy::HistoricalOnly
        {
            prefix_end.get_or_insert_with(|| builder.checkpoint());
        }
        position = row - 1;
        builder.push(
            event.kind,
            event.payload,
            event.timestamp,
            event.replay_policy,
            Some(event.record_type),
            None,
        );
        builder.set_last_database_step_hash(event.hash);
    }
    if position > 0 || require_contiguous_prefix {
        prefix_end.get_or_insert_with(|| builder.checkpoint());
    }
    prefix_end
}

fn database_step_order(connection: &rusqlite::Connection) -> Result<(&'static str, &'static str)> {
    let without_rowid: bool = connection.query_row(
        "SELECT wr FROM pragma_table_list('steps') WHERE schema = 'main' AND type = 'table'",
        [],
        |row| row.get(0),
    )?;
    if !without_rowid {
        return Ok(("_rowid_", "idx DESC, _rowid_ DESC"));
    }
    let indexed_by_step: bool = connection.query_row(
        "SELECT count(*) = 1 AND min(lower(name)) = 'idx' FROM pragma_table_info('steps') WHERE pk > 0",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        indexed_by_step,
        "Antigravity steps have neither a rowid nor an idx primary key"
    );
    Ok(("idx", "idx DESC"))
}

fn record_database_proof(
    aliases: &mut omnis_ir::antigravity_receipts::DatabaseAliases,
    builder: &EventBuilder,
    event: &mut DatabaseStepEvent,
    index: i64,
    wire: &[u8],
) {
    let opaque = event.replay_policy == ReplayPolicy::HistoricalOnly;
    let namespace = builder.database_step_namespace(index, opaque);
    let raw = uuid::Uuid::new_v5(&namespace, wire);
    event.hash = Some(raw);
    if !opaque
        && let Some(alias) =
            omnis_ir::antigravity_receipts::DatabaseStepAlias::from_wire(namespace, raw, wire)
        && alias.canonical != raw
    {
        aliases.insert(raw, alias);
    }
}

struct DatabaseStepEvent {
    hash: Option<uuid::Uuid>,
    kind: EventKind,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
    replay_policy: ReplayPolicy,
    record_type: String,
}

fn database_step_event(step: ProtoStep, wire: &[u8]) -> DatabaseStepEvent {
    let timestamp = step
        .metadata
        .and_then(|metadata| metadata.created_at)
        .as_ref()
        .and_then(proto_timestamp);
    let opaque = || {
        (
            EventKind::ProviderEvent,
            json!({"status": step.status.to_string(), "step_payload_base64": STANDARD.encode(wire)}),
            ReplayPolicy::HistoricalOnly,
            format!("STEP_TYPE_{}", step.r#type),
        )
    };
    let (kind, payload, replay_policy, record_type) = match step.step {
        Some(proto_step::Step::UserInput(input)) => {
            let text = user_input_text(&input);
            let query = antigravity_user_request(&text);
            if query.is_empty() {
                opaque()
            } else {
                (
                    EventKind::MessageUser,
                    json!({"text": query}),
                    ReplayPolicy::Contextual,
                    "USER_INPUT".to_owned(),
                )
            }
        }
        Some(proto_step::Step::PlannerResponse(response)) => {
            let text = if response.modified_response.is_empty() {
                response.response
            } else {
                response.modified_response
            };
            if text.is_empty() {
                opaque()
            } else {
                (
                    EventKind::MessageAssistant,
                    json!({"text": text}),
                    ReplayPolicy::Contextual,
                    "PLANNER_RESPONSE".to_owned(),
                )
            }
        }
        _ => opaque(),
    };
    DatabaseStepEvent {
        hash: None,
        kind,
        payload,
        timestamp,
        replay_policy,
        record_type,
    }
}

/// Antigravity CLI writes `query`. The desktop app leaves it empty and stores typed text in
/// `user_response` and text `items`.
fn user_input_text(input: &ProtoUserInput) -> Cow<'_, str> {
    [&input.query, &input.user_response]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(
            || Cow::Owned(input.items.iter().map(|item| item.text.as_str()).collect()),
            |text| Cow::Borrowed(text.as_str()),
        )
}

pub(crate) fn proto_timestamp(value: &ProtoTimestamp) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.seconds, u32::try_from(value.nanos).ok()?)
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoStep {
    #[prost(int32, tag = "1")]
    pub(crate) r#type: i32,
    #[prost(int32, tag = "4")]
    pub(crate) status: i32,
    #[prost(message, optional, tag = "5")]
    pub(crate) metadata: Option<ProtoStepMetadata>,
    #[prost(oneof = "proto_step::Step", tags = "19, 20")]
    pub(crate) step: Option<proto_step::Step>,
}

pub(crate) mod proto_step {
    use prost::Oneof;

    use super::{ProtoPlannerResponse, ProtoUserInput};

    #[derive(Clone, PartialEq, Oneof)]
    pub(crate) enum Step {
        #[prost(message, tag = "19")]
        UserInput(ProtoUserInput),
        #[prost(message, tag = "20")]
        PlannerResponse(ProtoPlannerResponse),
    }
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoStepMetadata {
    #[prost(message, optional, tag = "1")]
    pub(crate) created_at: Option<ProtoTimestamp>,
    #[prost(int32, tag = "3")]
    pub(crate) source: i32,
    #[prost(string, tag = "12")]
    pub(crate) execution_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    pub(crate) seconds: i64,
    #[prost(int32, tag = "2")]
    pub(crate) nanos: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoUserInput {
    #[prost(string, tag = "1")]
    pub(crate) query: String,
    #[prost(string, tag = "2")]
    pub(crate) user_response: String,
    #[prost(message, repeated, tag = "3")]
    pub(crate) items: Vec<ProtoTextOrScopeItem>,
}

/// Text chunk of `TextOrScopeItem`. Scope items (field 2) are skipped.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoTextOrScopeItem {
    #[prost(string, tag = "1")]
    pub(crate) text: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ProtoPlannerResponse {
    #[prost(string, tag = "1")]
    pub(crate) response: String,
    #[prost(string, tag = "8")]
    pub(crate) modified_response: String,
}

#[cfg(test)]
pub(crate) fn synthetic_step(step_type: i32, step: Option<proto_step::Step>) -> ProtoStep {
    ProtoStep {
        r#type: step_type,
        status: 3,
        metadata: Some(ProtoStepMetadata {
            created_at: Some(ProtoTimestamp {
                seconds: 1_785_240_000,
                nanos: 0,
            }),
            source: 0,
            execution_id: String::new(),
        }),
        step,
    }
}

/// Writes the `steps` table shared by Antigravity CLI and desktop app conversation databases.
#[cfg(test)]
pub(crate) fn write_steps_database(database: &Path, steps: &[ProtoStep]) {
    let connection = rusqlite::Connection::open(database).expect("conversation database");
    connection
        .execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER NOT NULL DEFAULT 0,
                step_payload BLOB
            );",
        )
        .expect("steps schema");
    for (index, step) in steps.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    i64::try_from(index).expect("index"),
                    step.r#type,
                    step.encode_to_vec()
                ],
            )
            .expect("step row");
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use omnis_ir::{EventKind, Provider, SessionRef};

    use super::{
        EventBuilder, ProtoPlannerResponse, ProtoTextOrScopeItem, ProtoUserInput,
        antigravity_user_request, proto_step, push_database_steps, synthetic_step, user_input_text,
        workspace_path, write_steps_database,
    };

    #[test]
    fn decodes_local_workspace_uri() {
        assert_eq!(
            workspace_path(r#"["file:///tmp/project%20space"]"#).as_deref(),
            Some(std::path::Path::new("/tmp/project space"))
        );
        assert!(workspace_path(r#"["file://remote/path"]"#).is_none());
    }

    #[test]
    fn unwraps_user_request_without_copying_runtime_metadata() {
        let wrapped = "<USER_REQUEST>\nquestion\n</USER_REQUEST>\n<ADDITIONAL_METADATA>synthetic metadata</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>synthetic settings</USER_SETTINGS_CHANGE>";

        assert_eq!(antigravity_user_request(wrapped), "question");
        assert_eq!(
            antigravity_user_request("<div>keep markup</div>"),
            "<div>keep markup</div>"
        );
    }

    #[test]
    fn reads_message_steps_from_native_database() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let database = temporary.path().join("conversation.db");
        write_steps_database(
            &database,
            &[
                synthetic_step(
                    14,
                    Some(proto_step::Step::UserInput(ProtoUserInput {
                        query: "question".to_owned(),
                        ..ProtoUserInput::default()
                    })),
                ),
                synthetic_step(
                    15,
                    Some(proto_step::Step::PlannerResponse(ProtoPlannerResponse {
                        response: "answer".to_owned(),
                        modified_response: String::new(),
                    })),
                ),
            ],
        );

        let assert_messages = |order_verified| {
            let session = SessionRef::new(
                Provider::Antigravity,
                "11111111-1111-4111-8111-111111111111",
            );
            let mut builder = EventBuilder::new(session.provider, &session.id);
            assert_eq!(
                push_database_steps(temporary.path(), &database, &mut builder, 2)
                    .unwrap()
                    .0,
                2
            );
            let snapshot = builder.snapshot(session, None, None, None, Utc::now());
            assert_eq!(snapshot.events.len(), if order_verified { 2 } else { 3 });
            if !order_verified {
                assert_eq!(
                    snapshot.events[2].payload["native_history_unverified"],
                    true
                );
            }
            assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
            assert_eq!(snapshot.events[0].payload["text"], "question");
            assert_eq!(snapshot.events[1].kind, EventKind::MessageAssistant);
            assert_eq!(snapshot.events[1].payload["text"], "answer");
        };
        assert_messages(true);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE restored AS SELECT * FROM steps ORDER BY idx;
             DROP TABLE steps;
             ALTER TABLE restored RENAME TO steps;
             UPDATE steps SET idx = 0;",
            )
            .unwrap();
        assert_messages(false);
        connection.execute_batch("UPDATE steps SET idx = _rowid_; INSERT INTO steps SELECT * FROM steps WHERE idx = 2;").unwrap();
        let mut builder = EventBuilder::new(Provider::Antigravity, "ordered-prefix");
        let (decoded, coverage) =
            push_database_steps(temporary.path(), &database, &mut builder, 3).unwrap();
        assert_eq!(
            (decoded, coverage.verified_prefix_end, coverage.omitted),
            (3, Some(1), 0)
        );
    }

    #[test]
    fn database_budgets_skip_large_blobs_and_report_history_omissions_without_mutation() {
        use prost::Message;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("conversation.db");
        let text = ["First visible request", "Final visible request"];
        let steps = text.map(|query| {
            synthetic_step(
                14,
                Some(proto_step::Step::UserInput(ProtoUserInput {
                    query: query.to_owned(),
                    ..ProtoUserInput::default()
                })),
            )
        });
        let wire_bytes = steps[1].encoded_len();
        write_steps_database(&database, &steps);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO steps (idx, step_payload) VALUES (2, zeroblob(4096))",
                [],
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&database).unwrap();
        let visible_bytes =
            crate::support::estimated_event_bytes(&serde_json::json!({"text": text[1]}));
        let row_limit = super::DatabaseReadLimits {
            row_bytes: wire_bytes,
            ..super::DatabaseReadLimits::default()
        };
        let session = SessionRef::new(
            Provider::Antigravity,
            "11111111-1111-4111-8111-111111111111",
        );
        for (limits, expected) in [
            (row_limit, &text[..]),
            (
                super::DatabaseReadLimits {
                    history_bytes: visible_bytes,
                    ..row_limit
                },
                &text[1..],
            ),
            (
                super::DatabaseReadLimits {
                    records: 2,
                    ..row_limit
                },
                &text[1..],
            ),
            (
                super::DatabaseReadLimits {
                    wire_bytes: steps.iter().map(Message::encoded_len).sum(),
                    ..super::DatabaseReadLimits::default()
                },
                &text[..],
            ),
            (
                super::DatabaseReadLimits {
                    wire_bytes,
                    ..super::DatabaseReadLimits::default()
                },
                &text[1..],
            ),
            (
                super::DatabaseReadLimits {
                    records: 1,
                    ..row_limit
                },
                &text[..0],
            ),
        ] {
            let mut builder = EventBuilder::new(session.provider, &session.id);
            let (decoded, coverage) = super::push_database_steps_with_limits(
                temporary.path(),
                &database,
                &mut builder,
                3,
                limits,
            )
            .unwrap();
            assert!(decoded >= expected.len());
            assert_eq!(
                coverage.verified_prefix_end,
                Some(if expected.len() == text.len() { 2 } else { 0 })
            );
            let snapshot = builder.snapshot(session.clone(), None, None, None, Utc::now());
            let (notice, messages) = snapshot.events.split_last().unwrap();
            let actual = messages
                .iter()
                .map(|message| message.payload["text"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
            assert_eq!(notice.payload["omitted_events"], 3 - expected.len());
            assert_eq!(notice.replay_policy, omnis_ir::ReplayPolicy::HistoricalOnly);
        }
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn damaged_database_steps_do_not_hide_a_readable_request() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("conversation.db");
        let id = "11111111-1111-4111-8111-111111111111";
        let request = synthetic_step(
            14,
            Some(proto_step::Step::UserInput(ProtoUserInput {
                query: "Latest readable request".into(),
                ..ProtoUserInput::default()
            })),
        );
        write_steps_database(&database, &[request]);
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 15, NULL), (2, 14, x'80'), (3, 14, 'invalid'), (4, 14, x'')",
        )
        .unwrap();
        let mut builder = EventBuilder::new(Provider::Antigravity, id);
        let (decoded, coverage) =
            push_database_steps(temporary.path(), &database, &mut builder, 6).unwrap();
        assert_eq!(
            (decoded, coverage.verified_prefix_end, coverage.omitted),
            (1, Some(1), 5)
        );
        let snapshot = builder.snapshot(
            SessionRef::new(Provider::Antigravity, id),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(
            snapshot.events[0].payload["text"],
            "Latest readable request"
        );
        assert_eq!(snapshot.events[1].payload["omitted_events"], 5);
        assert_eq!(snapshot.events[1].payload["declared_steps"], 6);
        assert_eq!(snapshot.events[1].payload["stored_steps"], 5);
        conn.execute("DELETE FROM steps WHERE idx = 0", []).unwrap();
        let mut builder = EventBuilder::new(Provider::Antigravity, id);
        assert!(push_database_steps(temporary.path(), &database, &mut builder, 6).is_err());
    }

    #[test]
    fn unknown_database_steps_keep_opaque_wire_history_and_report_unsupported_text() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct FutureStep {
            #[prost(int32, tag = "1")]
            kind: i32,
            #[prost(string, tag = "99")]
            future_text: String,
        }

        let wire = FutureStep {
            kind: 999,
            future_text: "Synthetic future native history".to_owned(),
        }
        .encode_to_vec();
        assert_opaque_database_step(&wire);
    }

    #[test]
    fn scope_only_user_steps_and_empty_responses_preserve_wire_history() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct ScopeItem {
            #[prost(bytes = "vec", tag = "2")]
            scope: Vec<u8>,
        }
        #[derive(Clone, PartialEq, Message)]
        struct ScopeInput {
            #[prost(message, repeated, tag = "3")]
            items: Vec<ScopeItem>,
        }
        #[derive(Clone, PartialEq, Message)]
        struct ScopeStep {
            #[prost(int32, tag = "1")]
            kind: i32,
            #[prost(message, optional, tag = "19")]
            input: Option<ScopeInput>,
        }
        let scope_only = ScopeStep {
            kind: 14,
            input: Some(ScopeInput {
                items: vec![ScopeItem { scope: vec![8, 1] }],
            }),
        }
        .encode_to_vec();
        assert_opaque_database_step(&scope_only);
        assert_opaque_database_step(
            &synthetic_step(
                15,
                Some(proto_step::Step::PlannerResponse(
                    ProtoPlannerResponse::default(),
                )),
            )
            .encode_to_vec(),
        );
    }

    fn assert_opaque_database_step(wire: &[u8]) {
        use base64::Engine;

        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("conversation.db");
        write_steps_database(&database, &[]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO steps (idx, step_payload) VALUES (0, ?1)",
                [wire],
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&database).unwrap();
        let mut builder = EventBuilder::new(
            Provider::Antigravity,
            "11111111-1111-4111-8111-111111111111",
        );
        let (decoded, coverage) =
            push_database_steps(temporary.path(), &database, &mut builder, 1).unwrap();
        assert_eq!(
            (decoded, coverage.verified_prefix_end, coverage.omitted),
            (1, Some(0), 0)
        );
        let snapshot = builder.snapshot(
            SessionRef::new(
                Provider::Antigravity,
                "11111111-1111-4111-8111-111111111111",
            ),
            None,
            None,
            None,
            Utc::now(),
        );
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].kind, EventKind::ProviderEvent);
        assert_eq!(
            snapshot.events[0].replay_policy,
            omnis_ir::ReplayPolicy::HistoricalOnly
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(
                    snapshot.events[0].payload["step_payload_base64"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            wire
        );
        assert_eq!(snapshot.events[1].payload["unsupported_text_records"], 1);
        assert!(omnis_core::import_conversation(&snapshot).truncated);
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn reads_user_text_from_query_then_response_then_items() {
        let items = vec![
            ProtoTextOrScopeItem {
                text: "first ".to_owned(),
            },
            ProtoTextOrScopeItem {
                text: "second".to_owned(),
            },
        ];
        let input = ProtoUserInput {
            query: "query".to_owned(),
            user_response: "response".to_owned(),
            items: items.clone(),
        };
        assert_eq!(user_input_text(&input), "query");
        let input = ProtoUserInput {
            query: String::new(),
            ..input
        };
        assert_eq!(user_input_text(&input), "response");
        let input = ProtoUserInput {
            items,
            ..ProtoUserInput::default()
        };
        assert_eq!(user_input_text(&input), "first second");
    }
}
