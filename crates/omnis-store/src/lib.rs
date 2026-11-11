//! Local `SQLite` persistence for task selection, session lineage, and bundles.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, hash_map::RandomState},
    fmt::{self, Write as _},
    fs,
    hash::{BuildHasher, Hasher},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use directories::BaseDirs;
use omnis_ir::{PortableBundle, Provider, SessionRef, SourceRevision, TransferMode};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior,
    functions::FunctionFlags,
    params,
    types::{Type, Value as SqlValue, ValueRef},
};
use search_query::{SEARCH_QUERY_MAX_CHARS, SearchQuery, SearchTerm};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub mod search_query;

const DATABASE_FILE_NAME: &str = "store.sqlite3";
/// Format of the store, kept in `PRAGMA user_version`.
///
/// Format 1 stores chunk text compressed. A version that finds a higher number does not open the
/// store, because it cannot tell what a later version changed. Versions before format 1 do not
/// read the number. They fail when they open a format 1 store, because the chunk table is a view.
const STORE_FORMAT: i64 = 1;
/// Page cache limit for one connection, as the negative KiB count `PRAGMA cache_size` takes.
///
/// The `SQLite` default of 2 MiB is smaller than one trajectory write batch, so a batch spilled
/// dirty pages to the write-ahead log and read them back before it committed.
const PAGE_CACHE_KIBIBYTES: i64 = -64 * 1024;
const TRAJECTORY_PHRASE_MAX_TOKENS: usize = 64;
const TRAJECTORY_QUERY_MAX_TOKEN_BYTES: usize = 256;
const TRAJECTORY_SEARCH_RESULT_LIMIT: usize = 512;
const TRAJECTORY_CHUNK_BYTE_LIMIT: usize = 64 * 1024;
const MAX_UTF8_BYTES_PER_CHARACTER: usize = 4;
/// Maximum number of trajectory documents accepted by one atomic write.
pub const MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS: usize = 16;
/// Maximum combined redacted-text size accepted by one atomic trajectory write.
pub const MAX_TRAJECTORY_WRITE_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Most chunk rows one `INSERT` statement writes.
///
/// FTS5 flushes its pending index to a new segment whenever a statement that fires the chunk
/// triggers begins. One statement per chunk therefore wrote one segment per chunk and paid for
/// merging them. Sixteen full chunks fill the pending index anyway, and sixteen small documents
/// are one write batch, so larger groups would only add distinct statements to prepare.
const TRAJECTORY_CHUNK_INSERT_ROWS: usize = 16;
/// Prepared statements one connection keeps. Chunk inserts alone use one statement per row count.
const PREPARED_STATEMENT_CACHE_CAPACITY: usize = 2 * TRAJECTORY_CHUNK_INSERT_ROWS;
/// Most bytes that two consecutive chunks share.
///
/// A chunk restarts [`SEARCH_QUERY_MAX_CHARS`] characters before the previous chunk ends, so any
/// text of up to that many characters lies whole inside at least one chunk. A quoted phrase holds
/// at most that many characters, and folding maps one character to one character, so every exact
/// occurrence fits in one chunk, where both the FTS prefilter and the exact check see it.
///
/// The overlap is counted in characters. A count in bytes must assume four bytes for each
/// character, which made every chunk repeat 16 KiB of the chunk before it. ASCII text needs 4 KiB.
///
/// The tokens of an unquoted word such as `foo.bar` match across any run of separator characters.
/// When that run is longer than the shared text, the tokens can lie in different chunks, and the
/// occurrence is not found. Tokens that far apart are not one word.
const TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES: usize =
    SEARCH_QUERY_MAX_CHARS * MAX_UTF8_BYTES_PER_CHARACTER;
// A chunk is at least half of the limit long. An occurrence that starts at the last shared
// character ends inside the next chunk when that half holds the overlap and the occurrence.
const _: () = assert!(TRAJECTORY_CHUNK_BYTE_LIMIT / 2 >= 2 * TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES);
/// Segments that FTS5 collects on one level of the index before it merges them.
///
/// Each commit of a write batch adds one segment. With the FTS5 default of 4, an index build
/// merged segments often and spent a large part of its time on that. A higher value merges less
/// often. Searches read a few more segments, which did not change their measured time.
const FTS_AUTOMERGE_SEGMENTS: i64 = 8;
/// SQL function behind exact phrase checks, registered by [`register_search_functions`].
const FOLDED_CONTAINS_FUNCTION: &str = "omnis_folded_contains";
/// SQL function that returns the text of a stored chunk, registered by
/// [`register_search_functions`]. The chunk view and the chunk triggers call it.
const CHUNK_TEXT_FUNCTION: &str = "omnis_chunk_text";
/// SQL function that tells whether a stored chunk can be read, registered by
/// [`register_search_functions`]. The triggers that record an unreadable chunk call it.
const CHUNK_READABLE_FUNCTION: &str = "omnis_chunk_readable";
/// First byte of a compressed chunk. The text length follows as four little-endian bytes, and
/// then one LZ4 block.
const CHUNK_CODEC_LZ4: u8 = 1;
/// Bytes before the LZ4 block of a compressed chunk: the codec byte and the text length.
const CHUNK_HEADER_BYTES: usize = 5;

/// A chunk as the store keeps it. Chunk text is most of the store, and LZ4 makes it less than
/// half as large. A chunk that LZ4 does not make smaller stays plain text, and so do the chunks
/// that versions before compression wrote.
enum StoredChunk<'text> {
    Plain(&'text str),
    Lz4(Vec<u8>),
}

fn encode_chunk(text: &str) -> StoredChunk<'_> {
    let Ok(length) = u32::try_from(text.len()) else {
        return StoredChunk::Plain(text);
    };
    let mut stored =
        vec![0; CHUNK_HEADER_BYTES + lz4_flex::block::get_maximum_output_size(text.len())];
    stored[0] = CHUNK_CODEC_LZ4;
    stored[1..CHUNK_HEADER_BYTES].copy_from_slice(&length.to_le_bytes());
    match lz4_flex::block::compress_into(text.as_bytes(), &mut stored[CHUNK_HEADER_BYTES..]) {
        Ok(block_length) if CHUNK_HEADER_BYTES + block_length < text.len() => {
            stored.truncate(CHUNK_HEADER_BYTES + block_length);
            // The buffer was sized for the worst case. A write holds all of its chunks at once.
            stored.shrink_to_fit();
            StoredChunk::Lz4(stored)
        }
        _ => StoredChunk::Plain(text),
    }
}

/// Most threads that compress the chunks of one write.
const CHUNK_ENCODE_THREADS: usize = 4;
/// Fewest chunks that justify one more thread. Fewer chunks compress faster than a thread starts.
const MIN_CHUNKS_PER_ENCODE_THREAD: usize = 4;

/// Encodes every chunk, in order.
///
/// The thread that writes the index sets the duration of an index build, and compression is
/// independent work for each chunk. The chunks of one write are therefore compressed on a few
/// threads, and the calling thread takes a share.
fn encode_chunks<'text>(chunks: &[&'text str]) -> Vec<StoredChunk<'text>> {
    let encode = |chunks: &[&'text str]| chunks.iter().map(|chunk| encode_chunk(chunk)).collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(CHUNK_ENCODE_THREADS)
        .min(chunks.len() / MIN_CHUNKS_PER_ENCODE_THREAD);
    if threads < 2 {
        return encode(chunks);
    }
    let mut shares = chunks.chunks(chunks.len().div_ceil(threads));
    let first = shares.next().unwrap_or_default();
    std::thread::scope(|scope| {
        // A share whose thread cannot start is encoded here instead.
        let later = shares
            .map(|share| {
                std::thread::Builder::new()
                    .spawn_scoped(scope, move || encode(share))
                    .map_err(|_| share)
            })
            .collect::<Vec<_>>();
        let mut encoded: Vec<StoredChunk<'text>> = encode(first);
        for share in later {
            let share: Vec<StoredChunk<'text>> = match share {
                Ok(thread) => thread
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                Err(share) => encode(share),
            };
            encoded.extend(share);
        }
        encoded
    })
}

/// Text of a stored chunk, or `None` when the value is not a chunk this version can read.
///
/// Plain text is returned as it is stored. A compressed chunk is decoded into `scratch`, which a
/// caller reuses for every row so that a query over many chunks does not allocate for each one.
/// The stored length is checked before it sizes the buffer, because the store is a local file
/// that another program could change.
fn chunk_text<'value>(
    value: ValueRef<'value>,
    scratch: &'value mut Vec<u8>,
) -> Option<&'value str> {
    match value {
        ValueRef::Text(text) => std::str::from_utf8(text).ok(),
        ValueRef::Blob(stored) => {
            let (header, block) = stored.split_at_checked(CHUNK_HEADER_BYTES)?;
            let length = usize::try_from(u32::from_le_bytes(header[1..].try_into().ok()?)).ok()?;
            if header[0] != CHUNK_CODEC_LZ4 || length > TRAJECTORY_CHUNK_BYTE_LIMIT {
                return None;
            }
            scratch.clear();
            scratch.resize(length, 0);
            let decoded = lz4_flex::block::decompress_into(block, scratch).ok()?;
            (decoded == length).then_some(())?;
            std::str::from_utf8(scratch).ok()
        }
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => None,
    }
}
const FTS_SNIPPET: &str = "snippet(session_trajectory_chunks_fts, 0, '', '', ' … ', 28)";
/// Index that maps a chunk to its trajectory without reading the chunk row.
///
/// A chunk row holds up to 64 KiB of text, so each row sits on its own pages. A search that
/// matches many chunks read one such page per match only to learn the trajectory.
const CHUNK_OWNER_INDEX: &str = "session_trajectory_chunk_owner";

// A confirmed file failure hides conversation text and titles until a verified read clears it.
const HIDDEN_FAILED_TRAJECTORY: &str = "trajectories.provider IN ('pi', 'omp')
    AND trajectories.origin = 'native' AND failed.provider = trajectories.provider
    AND failed.session_id = trajectories.session_id AND failed.source_file_id IS NOT NULL
    AND (failed.source_contents_id IS NOT NULL OR failed.source_file_id IS NOT trajectories.source_file_id
        OR failed.source_contents_id IS NOT trajectories.source_contents_id
        OR failed.source_updated_at IS NOT trajectories.source_updated_at
        OR failed.source_updated_submillis IS NOT trajectories.source_updated_submillis)";

// Discovery can select a replacement before its first transcript read succeeds.
const CONFLICTING_FILE_CATALOG: &str = "trajectories.provider IN ('pi', 'omp')
    AND trajectories.origin = 'native' AND catalog.provider = trajectories.provider
    AND catalog.session_id = trajectories.session_id
    AND (catalog.source_file_id IS NOT trajectories.source_file_id
        OR catalog.source_contents_id IS NOT trajectories.source_contents_id)";

/// Joins full-text matches to their eligible trajectories. A clause that does not read chunk
/// text takes each trajectory from [`CHUNK_OWNER_INDEX`].
fn trajectory_candidate_chunks_sql(clause: &TrajectoryClause) -> String {
    let chunk_source = if clause.exact_phrase.is_some() {
        // The exact phrase check reads the chunk text, so the row is needed anyway.
        "session_trajectory_chunk_rows AS chunks".to_owned()
    } else {
        format!("session_trajectory_chunk_rows AS chunks INDEXED BY {CHUNK_OWNER_INDEX}")
    };
    format!(
        "FROM session_trajectory_chunks_fts
         INNER JOIN {chunk_source}
             ON chunks.id = session_trajectory_chunks_fts.rowid
         INNER JOIN session_trajectories AS trajectories
             ON trajectories.id = chunks.trajectory_id
         INNER JOIN eligible
             ON eligible.provider = trajectories.provider
            AND eligible.session_id = trajectories.session_id
            AND (eligible.file IS NULL OR (eligible.file = hex(trajectories.source_file_id)
                 AND eligible.contents = hex(trajectories.source_contents_id)))
            AND (eligible.file IS NOT NULL OR NOT EXISTS (
                 SELECT 1 FROM session_index AS catalog WHERE {CONFLICTING_FILE_CATALOG}))
            AND NOT EXISTS (SELECT 1 FROM trajectory_index_failures AS failed
                            WHERE {HIDDEN_FAILED_TRAJECTORY})"
    )
}
const EXACT_SNIPPET_LEADING_BYTES: usize = 80;
const EXACT_SNIPPET_TRAILING_BYTES: usize = 120;
const UPSERT_TRAJECTORY_PARENT_SQL: &str = "
    INSERT INTO session_trajectories (
        provider, session_id, redacted_text, content_hash,
        source_updated_at, source_complete,
        complete, indexed_at, source_byte_count, indexed_byte_count,
        truncation_strategy, origin, protected_by_bundle, document_version, derived_title,
        source_updated_submillis, provider_limited, title_authoritative,
        source_file_id, missing_ancestor_id, source_contents_id
    ) VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?16, ?17, ?18, ?20, ?21, ?22)
    ON CONFLICT (provider, session_id) DO UPDATE SET
        content_hash = excluded.content_hash,
        source_updated_at = excluded.source_updated_at,
        source_updated_submillis = excluded.source_updated_submillis,
        source_file_id = excluded.source_file_id,
        missing_ancestor_id = excluded.missing_ancestor_id,
        source_contents_id = excluded.source_contents_id,
        source_complete = excluded.source_complete,
        provider_limited = excluded.provider_limited,
        title_authoritative = excluded.title_authoritative,
        complete = excluded.complete,
        indexed_at = excluded.indexed_at,
        source_byte_count = excluded.source_byte_count,
        indexed_byte_count = excluded.indexed_byte_count,
        truncation_strategy = excluded.truncation_strategy,
        origin = excluded.origin,
        protected_by_bundle = max(
            session_trajectories.protected_by_bundle,
            excluded.protected_by_bundle
        ),
        document_version = excluded.document_version,
        derived_title = CASE
            WHEN excluded.source_file_id IS NOT NULL AND session_trajectories.source_file_id IS NOT NULL
                AND (excluded.source_file_id IS NOT session_trajectories.source_file_id
                     OR excluded.source_contents_id IS NOT session_trajectories.source_contents_id)
                THEN excluded.derived_title
            -- A new OMP reader must verify names guessed by an older sample.
            WHEN excluded.provider = 'omp'
                AND excluded.document_version > session_trajectories.document_version
                AND NOT session_trajectories.source_complete
                AND NOT coalesce(session_trajectories.title_authoritative, 0)
                THEN excluded.derived_title
            WHEN ?18 AND (
                ?15 OR excluded.document_version > session_trajectories.document_version
                OR session_trajectories.source_complete = 0
            ) THEN excluded.derived_title
            ELSE coalesce(excluded.derived_title, session_trajectories.derived_title)
        END
    WHERE excluded.document_version >= session_trajectories.document_version
      AND (excluded.provider NOT IN ('pi','omp') OR excluded.origin <> 'native'
           OR session_trajectories.origin <> 'native' OR (
               (excluded.source_file_id IS NOT NULL OR session_trajectories.source_file_id IS NULL)
               AND (excluded.source_file_id IS NULL OR session_trajectories.source_file_id IS NOT NULL
                    OR NOT session_trajectories.source_complete
                    OR ((excluded.source_complete OR excluded.provider_limited) AND
                       (?23 OR NOT EXISTS (SELECT 1 FROM session_index AS catalog
                            WHERE catalog.provider = excluded.provider AND catalog.session_id = excluded.session_id
                              AND catalog.source_file_id IS NULL))))))
      AND (
          (excluded.source_complete, excluded.provider_limited) >=
              (session_trajectories.source_complete, session_trajectories.provider_limited)
          OR (excluded.provider_limited AND (?15 OR excluded.source_updated_at >= session_trajectories.source_updated_at))
          OR (excluded.document_version = session_trajectories.document_version
              AND (NOT ?15 OR session_trajectories.source_updated_submillis IS NOT NULL))
      ) AND (
       excluded.document_version > session_trajectories.document_version
       OR (excluded.source_updated_at, excluded.source_updated_submillis)
           > (session_trajectories.source_updated_at, coalesce(session_trajectories.source_updated_submillis, 0))
       OR (?15 AND excluded.source_file_id IS NOT NULL AND (
           excluded.source_file_id IS NOT session_trajectories.source_file_id
           OR excluded.source_contents_id IS NOT session_trajectories.source_contents_id
           OR excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id
       ))
       OR (?15 AND (
           excluded.source_updated_at IS NOT session_trajectories.source_updated_at
           OR excluded.source_updated_submillis IS NOT session_trajectories.source_updated_submillis
       ))
       OR (
           (excluded.source_updated_at, excluded.source_updated_submillis)
           = (session_trajectories.source_updated_at, coalesce(session_trajectories.source_updated_submillis, 0))
           AND (
               (excluded.source_complete, excluded.provider_limited) >
                   (session_trajectories.source_complete, session_trajectories.provider_limited)
               OR (
                   (excluded.source_complete, excluded.provider_limited) =
                       (session_trajectories.source_complete, session_trajectories.provider_limited)
                   AND (
                       excluded.complete > session_trajectories.complete
                       OR (
                           -- The verified replacement can exceed projection limits even when the old file fit.
                           (excluded.complete = session_trajectories.complete OR ?19)
                           AND (
                               excluded.content_hash <> session_trajectories.content_hash
                               OR (session_trajectories.tail_reader_version = 0 AND ?24)
                               OR excluded.source_byte_count <>
                                   session_trajectories.source_byte_count
                               OR excluded.indexed_byte_count <>
                                   session_trajectories.indexed_byte_count
                               OR excluded.truncation_strategy <>
                                   session_trajectories.truncation_strategy
                               OR excluded.origin <> session_trajectories.origin
                               OR session_trajectories.title_authoritative IS NULL
                               OR (excluded.title_authoritative <> session_trajectories.title_authoritative
                                   AND (excluded.title_authoritative OR ?19))
                               OR (?15 AND ?18 AND excluded.derived_title IS NOT
                                   session_trajectories.derived_title)
                           )
                       )
                   )
           )
       )
    ))
    RETURNING id
";
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database operation failed")]
    Database(#[source] DatabaseFailure),
    #[error("default data directory is unavailable")]
    DefaultDirectoryUnavailable,
    #[error("invalid task name")]
    InvalidTaskName,
    #[error("invalid branch name")]
    InvalidBranchName,
    #[error("invalid session reference")]
    InvalidSessionReference,
    #[error("trajectory write batch exceeds safe bounds")]
    TrajectoryBatchTooLarge,
    #[error("indexed source changed during the read")]
    TrajectorySourceChanged,
    #[error("a full read rejected this file revision")]
    TrajectoryPreviewRejected,
    #[error("workspace root must have a lossless UTF-8 representation")]
    InvalidWorkspaceRoot,
    #[error("task not found")]
    TaskNotFound,
    #[error("stored data is invalid")]
    CorruptStore,
    #[error("bundle encoding failed")]
    BundleEncoding,
    #[error("bundle already exists")]
    BundleAlreadyExists,
    #[error("store was written by a newer version of OmniSession")]
    NewerStoreFormat,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Content-free cause of a failed store database operation.
///
/// Only numeric `SQLite` result codes and coarse categories are kept, so error text never carries
/// transcript content, SQL parameters, or filesystem paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DatabaseFailure {
    /// `SQLite` rejected the operation with a primary and extended result code.
    Sqlite {
        primary_code: i32,
        extended_code: i32,
    },
    /// The `SQLite` interface failed without a result code, such as a value conversion.
    Interface,
    /// The state directory or database file could not be created or validated.
    Filesystem,
}

impl fmt::Display for DatabaseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Sqlite {
                primary_code,
                extended_code,
            } => write!(
                formatter,
                "SQLite error {primary_code} (extended code {extended_code}): {}",
                rusqlite::ffi::code_to_str(extended_code)
            ),
            Self::Interface => formatter.write_str("SQLite interface error without a result code"),
            Self::Filesystem => {
                formatter.write_str("state directory or database file could not be validated")
            }
        }
    }
}

impl std::error::Error for DatabaseFailure {}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used directly as a `map_err` adapter for owned rusqlite errors"
)]
fn database_error(error: rusqlite::Error) -> StoreError {
    StoreError::Database(match error {
        rusqlite::Error::SqliteFailure(failure, _) => DatabaseFailure::Sqlite {
            primary_code: failure.extended_code & 0xff,
            extended_code: failure.extended_code,
        },
        _ => DatabaseFailure::Interface,
    })
}

const fn filesystem_error() -> StoreError {
    StoreError::Database(DatabaseFailure::Filesystem)
}

/// A task scoped to one workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRecord {
    pub id: i64,
    pub name: String,
    pub workspace_root: PathBuf,
    pub created_at: DateTime<Utc>,
}

/// One entry in a task branch's session lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingRecord {
    pub id: i64,
    pub task_id: i64,
    pub branch_name: String,
    pub session: SessionRef,
    pub bound_at: DateTime<Utc>,
    pub is_current: bool,
}

/// Outcome of undoing the task branch head bound to a rolled-back session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchHeadRestore {
    /// The session was still the head. The binding it replaced, if any, is the head again.
    Restored(Option<SessionRef>),
    /// Another binding moved the head first, so nothing changed.
    Moved,
}

/// Outcome of advancing a task branch head that was read earlier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchHeadAdvance {
    /// The binding was still the head. The handoff is recorded and the target is the new head.
    Advanced(BindingRecord),
    /// Another binding moved the head first, so nothing changed.
    Moved,
}

/// A source-to-target session handoff, ordered by creation time in lineage views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffRecord {
    pub source: SessionRef,
    pub target: SessionRef,
    pub mode: TransferMode,
    pub created_at: DateTime<Utc>,
}

/// The provider catalog state observed before native discovery starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogObservation {
    provider: Provider,
    generation: i64,
}

/// Catalog rows and previously listed IDs captured before repeating a conflicting discovery.
#[derive(Debug)]
pub struct CatalogRetry {
    observation: CatalogObservation,
    rows: HashMap<String, CatalogRowState>,
    listed: HashMap<String, usize>,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct CatalogRowState {
    metadata: Option<IndexedSession>,
    failure: Option<TrajectoryIndexFailure>,
}

impl CatalogRetry {
    /// Returns the provider generation captured with the rows.
    #[must_use]
    pub const fn observation(&self) -> CatalogObservation {
        self.observation
    }
}

/// Cached provider metadata used to render session discovery without transcript reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedSession {
    pub session: SessionRef,
    /// The selected native file when the provider can identify it.
    pub source_revision: Option<SourceRevision>,
    pub title: Option<String>,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_at_approximate: bool,
    pub event_count: usize,
}

/// One ranked full-text session match with bounded redacted context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTrajectoryMatch {
    pub session: SessionRef,
    pub snippet: String,
    pub source_complete: bool,
    pub complete: bool,
    pub indexed_byte_count: usize,
    pub source_byte_count: usize,
    pub truncation_strategy: String,
}

/// One bounded page of stable ranked full-text matches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTrajectorySearchPage {
    pub matches: Vec<SessionTrajectoryMatch>,
    pub has_more: bool,
}

/// Provenance of one cached trajectory document's current content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTrajectoryOrigin {
    Native,
    ImportedBundle,
}

impl SessionTrajectoryOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::ImportedBundle => "imported_bundle",
        }
    }
}

/// One redacted search document with coverage, provenance, and version metadata.
#[derive(Clone, Copy, Debug)]
pub struct TrajectoryDocument<'a> {
    pub source_revision: SourceRevision,
    pub redacted_text: &'a str,
    pub source_updated_at: DateTime<Utc>,
    pub source_byte_count: usize,
    pub indexed_byte_count: usize,
    pub truncation_strategy: &'a str,
    /// Whether the document came from a full source read rather than a preview, even when the
    /// provider reported omitted events.
    pub source_complete: bool,
    /// The adapter sampled the source after reaching its full-read limit.
    pub provider_limited: bool,
    pub origin: SessionTrajectoryOrigin,
    pub document_version: u32,
    pub derived_title: Option<&'a str>,
    /// The provider reader established the current task title even when the transcript was sampled.
    pub title_authoritative: bool,
}

impl TrajectoryDocument<'_> {
    /// A full read can establish that a title was removed even if the text index sampled long messages.
    fn can_clear_derived_title(&self) -> bool {
        self.title_authoritative
            || (self.source_complete
                && matches!(
                    self.truncation_strategy,
                    "none"
                        | "event_head_tail"
                        | "document_head_tail"
                        | "event_and_document_head_tail"
                ))
    }
}

/// Restricts a write to the source state the reader saw before opening the transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectorySourceGuard {
    /// The provider orders updates by source time.
    NewerSource,
    /// A restored file may have any modification time. Only replace the source the reader saw.
    Observed(Option<DateTime<Utc>>),
    /// A previous indexer recorded only the millisecond that contained the modification time.
    ObservedLegacy(DateTime<Utc>),
    /// The indexed document observed before reading a source whose update time can repeat.
    ObservedDocument(Option<[u8; 32]>),
    /// The document and catalog observed before opening one native session.
    Verified {
        state: [u8; 32],
        known_files: [Option<Uuid>; 3],
    },
}

impl TrajectorySourceGuard {
    /// Whether a native reader identifies a search source by its file modification time.
    #[must_use]
    pub const fn uses_file_time(provider: Provider) -> bool {
        matches!(provider, Provider::Pi | Provider::OhMyPi)
    }

    /// Whether the stored document, catalog, or failure identifies another physical file for this session.
    #[must_use]
    pub fn has_competing_file(self, file: Option<Uuid>) -> bool {
        matches!((self, file), (Self::Verified { known_files, .. }, Some(file)) if known_files.into_iter().flatten().any(|known| known != file))
    }

    fn check(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        source: DateTime<Utc>,
    ) -> Result<()> {
        let prior = match self {
            Self::NewerSource => return Ok(()),
            Self::ObservedDocument(expected) => {
                return if expected == document_receipt(transaction, session)? {
                    Ok(())
                } else {
                    Err(StoreError::TrajectorySourceChanged)
                };
            }
            Self::Verified { .. } => {
                return if self == verified_session_state(transaction, session)? {
                    Ok(())
                } else {
                    Err(StoreError::TrajectorySourceChanged)
                };
            }
            Self::Observed(prior) => prior.map(|time| {
                (
                    time.timestamp_millis(),
                    Some(source_submillis(session.provider, time)),
                )
            }),
            Self::ObservedLegacy(time) => Some((time.timestamp_millis(), None)),
        };
        let mut statement = transaction
            .prepare_cached("SELECT source_updated_at, source_updated_submillis FROM session_trajectories WHERE provider = ?1 AND session_id = ?2")
            .map_err(database_error)?;
        let current = statement
            .query_row(params![session.provider.to_string(), session.id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<u32>>(1)?))
            })
            .optional()
            .map_err(database_error)?;
        // Concurrent reads of the same file can still improve coverage.
        if current == prior
            || current
                == Some((
                    source.timestamp_millis(),
                    Some(source_submillis(session.provider, source)),
                ))
        {
            Ok(())
        } else {
            Err(StoreError::TrajectorySourceChanged)
        }
    }
}

fn samples_jsonl_tail(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Codex
            | Provider::Claude
            | Provider::Grok
            | Provider::Antigravity
            | Provider::AntigravityIde
            | Provider::AntigravityDesktop
            | Provider::Pi
            | Provider::OhMyPi
    )
}

/// Indexing state of one cached search document, used to skip unchanged sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrajectoryIndexState {
    /// Version of the tail reader that produced this native JSONL preview.
    pub jsonl_tail_reader_version: Option<u32>,
    pub source_revision: SourceRevision,
    pub source_updated_at: DateTime<Utc>,
    /// False for historical rows written before the index recorded file time below a millisecond.
    pub source_time_precise: bool,
    pub source_complete: bool,
    /// The adapter exhausted its full-read budget and returned its best available preview.
    pub provider_limited: bool,
    /// Whether this document verified the title. Older writers left no evidence.
    pub title_authoritative: Option<bool>,
    pub document_version: u32,
}

/// A derived title and the document revision that verified it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexedTitle {
    pub source_revision: SourceRevision,
    pub title: Option<String>,
    pub verified_at: Option<(u32, DateTime<Utc>)>,
    /// The timestamp of the cached listing that matched this document.
    pub cached_at: Option<DateTime<Utc>>,
}

/// A session eligible for conversation search, optionally restricted to a discovered rollout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrajectorySelection {
    pub session: SessionRef,
    pub source: Option<SourceRevision>,
}

/// A session whose source could not be read for indexing, recorded so later passes can skip it
/// until the source or the search document version changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrajectoryIndexFailure {
    pub source_file: Option<Uuid>,
    pub source_contents: Option<Uuid>,
    /// Source update time discovered when the read failed, when the provider reports one.
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Whether the indexer retained the full precision of the source time.
    pub source_time_precise: bool,
    /// Search document version of the indexer whose read failed.
    pub document_version: u32,
}

impl TrajectoryIndexFailure {
    /// Whether the reader selected the exact file and contents that failed.
    #[must_use]
    pub fn matches_file_revision(self, revision: SourceRevision) -> bool {
        revision.file.is_some()
            && revision.contents.is_some()
            && self.source_file == revision.file
            && self.source_contents == revision.contents
    }
}

fn index_failure(
    connection: &Connection,
    session: &SessionRef,
) -> Result<Option<TrajectoryIndexFailure>> {
    connection
        .prepare_cached(
            "SELECT source_updated_at, document_version, source_updated_submillis,
                    source_file_id, source_contents_id FROM trajectory_index_failures
             WHERE provider = ?1 AND session_id = ?2",
        )
        .map_err(database_error)?
        .query_row(params![session.provider.to_string(), session.id], |row| {
            failure_from_row(row, 0)
        })
        .optional()
        .map_err(database_error)
}

/// One search document, the prior state its reader observed, and any catalog read from the same file.
pub type SelectedTrajectoryDocument<'s, 'd> = (
    &'s SessionRef,
    TrajectoryDocument<'d>,
    TrajectorySourceGuard,
    Option<&'s IndexedSession>,
);

struct PreparedTrajectoryWrite<'session, 'document> {
    session: &'session SessionRef,
    document: TrajectoryDocument<'document>,
    verified_metadata: Option<&'session IndexedSession>,
    content_hash: Vec<u8>,
    guard: TrajectorySourceGuard,
    source_complete: bool,
    complete: bool,
    source_byte_count: i64,
    indexed_byte_count: i64,
}

impl<'s, 'd> PreparedTrajectoryWrite<'s, 'd> {
    fn new(
        (session, document, guard, verified_metadata): SelectedTrajectoryDocument<'s, 'd>,
        remaining_bytes: usize,
    ) -> Result<Self> {
        validate_session_ref(session)?;
        if let Some(catalog) = verified_metadata {
            validate_selected_catalog(session, document, guard, catalog)?;
        }
        if (document.source_complete && document.provider_limited)
            || document.indexed_byte_count != document.redacted_text.len()
            || !valid_truncation_strategy(document.truncation_strategy)
            || !valid_trajectory_coverage(
                document.source_byte_count,
                document.indexed_byte_count,
                document.truncation_strategy,
            )
        {
            return Err(StoreError::InvalidSessionReference);
        }
        if document.indexed_byte_count > remaining_bytes {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }
        // A full read can cover a source even when the provider omitted events.
        let source_complete =
            document.source_complete && document.truncation_strategy != "legacy_unknown";
        let complete = source_complete
            && document.truncation_strategy == "none"
            && document.source_byte_count == document.indexed_byte_count;
        Ok(Self {
            session,
            content_hash: Sha256::digest(document.redacted_text.as_bytes()).to_vec(),
            guard,
            document,
            verified_metadata,
            source_complete,
            complete,
            source_byte_count: i64::try_from(document.source_byte_count)
                .map_err(|_| StoreError::InvalidSessionReference)?,
            indexed_byte_count: i64::try_from(document.indexed_byte_count)
                .map_err(|_| StoreError::InvalidSessionReference)?,
        })
    }

    fn observed_full_source(&self) -> bool {
        self.source_complete
            && self.document.origin == SessionTrajectoryOrigin::Native
            && matches!(self.guard, TrajectorySourceGuard::ObservedDocument(_))
    }

    fn should_publish(&self, transaction: &Transaction<'_>) -> Result<bool> {
        let revision = self.document.source_revision;
        let guard = self
            .guard
            .check(transaction, self.session, self.document.source_updated_at);
        // A stored sample can already match this write when a full read fails.
        // Check the failure before accepting either the sample or an idempotent write.
        let failure = if self.document.origin == SessionTrajectoryOrigin::Native
            && revision.contents.is_some()
            && (!self.source_complete || guard.is_err())
        {
            index_failure(transaction, self.session)?
        } else {
            None
        };
        if !self.source_complete
            && failure.is_some_and(|failed| failed.matches_file_revision(revision))
        {
            return Err(StoreError::TrajectoryPreviewRejected);
        }
        if let (Err(StoreError::TrajectorySourceChanged), Some(metadata)) =
            (&guard, self.verified_metadata)
            && failure.is_none()
            && self.matches_stored(transaction)?
            && matches_catalog_row(transaction, metadata)?
        {
            return Ok(false);
        }
        guard.map(|()| true)
    }

    fn matches_stored(&self, connection: &Connection) -> Result<bool> {
        let doc = self.document;
        connection
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM session_trajectories
                    WHERE provider = ?1 AND session_id = ?2 AND content_hash = ?3
                      AND source_updated_at = ?4 AND source_updated_submillis = ?5
                      AND source_complete = ?6 AND provider_limited = ?7 AND complete = ?8
                      AND source_byte_count = ?9 AND indexed_byte_count = ?10
                      AND truncation_strategy = ?11 AND origin = ?12 AND document_version = ?13
                      AND (NOT ?14 OR derived_title IS ?15)
                      AND title_authoritative IS ?16 AND source_file_id IS ?17
                      AND missing_ancestor_id IS ?18 AND source_contents_id IS ?19
                )",
                params![
                    self.session.provider.to_string(),
                    self.session.id,
                    self.content_hash,
                    doc.source_updated_at.timestamp_millis(),
                    source_submillis(self.session.provider, doc.source_updated_at),
                    self.source_complete,
                    doc.provider_limited,
                    self.complete,
                    self.source_byte_count,
                    self.indexed_byte_count,
                    doc.truncation_strategy,
                    doc.origin.as_str(),
                    doc.document_version,
                    doc.can_clear_derived_title() || doc.derived_title.is_some(),
                    doc.derived_title,
                    doc.can_clear_derived_title(),
                    doc.source_revision.file.map(Uuid::into_bytes),
                    doc.source_revision.missing_ancestor.map(Uuid::into_bytes),
                    doc.source_revision.contents.map(Uuid::into_bytes),
                ],
                |row| row.get(0),
            )
            .map_err(database_error)
    }
}

fn validate_selected_catalog(
    session: &SessionRef,
    document: TrajectoryDocument<'_>,
    guard: TrajectorySourceGuard,
    catalog: &IndexedSession,
) -> Result<()> {
    let revision = catalog.source_revision.unwrap_or_default();
    if &catalog.session != session
        || !TrajectorySourceGuard::uses_file_time(session.provider)
        || !matches!(guard, TrajectorySourceGuard::Verified { .. })
        || document.origin != SessionTrajectoryOrigin::Native
        || !(document.source_complete
            || (document.source_revision.file.is_some()
                && document.source_revision.contents.is_some()))
        || revision.file != document.source_revision.file
        || revision.contents != document.source_revision.contents
        || document.truncation_strategy == "legacy_unknown"
        || catalog.updated_at != Some(document.source_updated_at)
    {
        return Err(StoreError::InvalidSessionReference);
    }
    Ok(())
}

const DOCUMENT_RECEIPT_SQL: &str =
    "SELECT json_array(id, indexed_at, hex(content_hash), source_updated_at,
         source_updated_submillis, source_complete, provider_limited, complete,
         document_version, source_byte_count, indexed_byte_count, truncation_strategy,
         origin, derived_title, title_authoritative, hex(source_file_id), hex(missing_ancestor_id),
         hex(source_contents_id))
     FROM session_trajectories WHERE provider = ?1 AND session_id = ?2";

fn document_receipt(connection: &Connection, session: &SessionRef) -> Result<Option<[u8; 32]>> {
    connection
        .prepare_cached(DOCUMENT_RECEIPT_SQL)
        .map_err(database_error)?
        .query_row(params![session.provider.to_string(), session.id], |row| {
            Ok(Sha256::digest(row.get::<_, String>(0)?).into())
        })
        .optional()
        .map_err(database_error)
}

fn verified_session_state(
    connection: &Connection,
    session: &SessionRef,
) -> Result<TrajectorySourceGuard> {
    observed_file_state(connection, session, None).map(|(state, _)| state)
}

fn observed_file_state(
    connection: &Connection,
    session: &SessionRef,
    file: Option<Uuid>,
) -> Result<(TrajectorySourceGuard, bool)> {
    // A restored file can reuse its modification time. Observe content, coverage and catalog
    // together so another full reader cannot restore an obsolete title or workspace.
    let sql = format!(
        "SELECT json_array(?1, ?2, ({DOCUMENT_RECEIPT_SQL}),
            CASE WHEN catalog.provider IS NOT NULL THEN json_array(catalog.title, catalog.project_path,
                catalog.git_branch, catalog.created_at, catalog.updated_at, catalog.updated_at_approximate,
                catalog.event_count, catalog.source_updated_submillis, hex(catalog.source_file_id),
                hex(catalog.source_contents_id)) END,
            CASE WHEN failed.provider IS NOT NULL THEN json_array(failed.source_updated_at,
                failed.source_updated_submillis, failed.document_version, hex(failed.source_file_id),
                hex(failed.source_contents_id)) END),
            doc.source_file_id, catalog.source_file_id,
            CASE WHEN failed.source_contents_id IS NOT NULL THEN failed.source_file_id END
         FROM (SELECT ?1 AS provider, ?2 AS session_id) AS selected
         LEFT JOIN session_trajectories AS doc USING (provider, session_id)
         LEFT JOIN session_index AS catalog USING (provider, session_id)
         LEFT JOIN trajectory_index_failures AS failed USING (provider, session_id)"
    );
    let guard = connection
        .prepare_cached(&sql)
        .map_err(database_error)?
        .query_row(params![session.provider.to_string(), session.id], |row| {
            let state: String = row.get(0)?;
            Ok(TrajectorySourceGuard::Verified {
                state: Sha256::digest(state.as_bytes()).into(),
                known_files: [
                    optional_uuid(row, 1)?,
                    optional_uuid(row, 2)?,
                    optional_uuid(row, 3)?,
                ],
            })
        })
        .map_err(database_error)?;
    Ok((guard, guard.has_competing_file(file)))
}

fn matches_catalog_row(connection: &Connection, metadata: &IndexedSession) -> Result<bool> {
    let project = metadata
        .project_path
        .as_deref()
        .map(|path| path.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
        .transpose()?;
    let event_count =
        i64::try_from(metadata.event_count).map_err(|_| StoreError::InvalidSessionReference)?;
    connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM session_index WHERE provider = ?1 AND session_id = ?2
         AND title IS ?3 AND project_path IS ?4 AND git_branch IS ?5 AND created_at IS ?6
         AND updated_at IS ?7 AND updated_at_approximate = ?8 AND event_count = ?9 AND source_updated_submillis IS ?10
         AND source_file_id IS ?11 AND source_contents_id IS ?12)",
            params![
                metadata.session.provider.to_string(),
                metadata.session.id,
                metadata.title,
                project,
                metadata.git_branch,
                metadata.created_at.map(|time| time.timestamp_millis()),
                metadata.updated_at.map(|time| time.timestamp_millis()),
                metadata.updated_at_approximate,
                event_count,
                catalog_submillis(metadata),
                metadata.source_revision.and_then(|r| r.file).map(Uuid::into_bytes),
                metadata.source_revision.and_then(|r| r.contents).map(Uuid::into_bytes),
            ],
            |row| row.get(0),
        )
        .map_err(database_error)
}

/// `SQLite`-backed state for one local `OmniSession` installation.
pub struct Store {
    connection: RefCell<Connection>,
}

impl Store {
    /// Opens a store at `path`, creating its schema when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open, configure, or initialize the store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        reject_symlink(path)?;
        let mut connection = Connection::open(path).map_err(database_error)?;
        register_search_functions(&connection).map_err(database_error)?;
        connection
            .busy_handler(Some(retry_busy_database))
            .map_err(database_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(database_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(database_error)?;
        connection.set_prepared_statement_cache_capacity(PREPARED_STATEMENT_CACHE_CAPACITY);
        connection
            .pragma_update(None, "cache_size", PAGE_CACHE_KIBIBYTES)
            .map_err(database_error)?;
        connection
            .pragma_update(None, "temp_store", "MEMORY")
            .map_err(database_error)?;

        let store = Self {
            connection: RefCell::new(connection),
        };
        store.initialize_schema()?;
        set_private_file(path)?;
        for suffix in ["-wal", "-shm"] {
            let mut auxiliary = path.as_os_str().to_os_string();
            auxiliary.push(suffix);
            let auxiliary = PathBuf::from(auxiliary);
            if auxiliary.exists() {
                set_private_file(&auxiliary)?;
            }
        }
        Ok(store)
    }

    /// Opens `OMNISESSION_HOME` or `~/.omnisession` as state root.
    ///
    /// # Errors
    ///
    /// Returns an error when state-root creation, validation, or store initialization fails.
    pub fn open_default() -> Result<Self> {
        Self::open_state_root(&state_root()?)
    }

    /// Creates, selects, and optionally binds a task in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or failed persistence.
    pub fn start_task(
        &self,
        name: impl AsRef<str>,
        workspace_root: impl AsRef<Path>,
        branch_name: impl AsRef<str>,
        session: Option<&SessionRef>,
    ) -> Result<TaskRecord> {
        let name = name.as_ref();
        let branch_name = branch_name.as_ref();
        if name.trim().is_empty() {
            return Err(StoreError::InvalidTaskName);
        }
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        if let Some(session) = session {
            validate_session_ref(session)?;
        }
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let timestamp = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "INSERT INTO tasks (name, workspace_root, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (workspace_root, name) DO NOTHING",
                params![name, workspace_root, timestamp],
            )
            .map_err(database_error)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::CorruptStore)?;
        transaction
            .execute(
                "INSERT INTO task_selections (workspace_root, task_id, selected_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (workspace_root) DO UPDATE SET
                   task_id = excluded.task_id, selected_at = excluded.selected_at",
                params![workspace_root, task.id, timestamp],
            )
            .map_err(database_error)?;
        if let Some(session) = session {
            replace_branch_head(&transaction, task.id, branch_name, session, timestamp)?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(task)
    }

    /// Creates a task or returns its existing record for this workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty task name or a failed database operation.
    pub fn create_or_get_task(
        &self,
        name: impl AsRef<str>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<TaskRecord> {
        let name = name.as_ref();
        if name.trim().is_empty() {
            return Err(StoreError::InvalidTaskName);
        }
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "
                INSERT INTO tasks (name, workspace_root, created_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_root, name) DO NOTHING
                ",
                params![name, workspace_root, created_at],
            )
            .map_err(database_error)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::CorruptStore)?;
        transaction.commit().map_err(database_error)?;
        Ok(task)
    }

    /// Makes a workspace task selected and returns it.
    ///
    /// # Errors
    ///
    /// Returns an error when the named workspace task does not exist or persistence fails.
    pub fn select_task(
        &self,
        workspace_root: impl AsRef<Path>,
        name: impl AsRef<str>,
    ) -> Result<TaskRecord> {
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let name = name.as_ref();
        let selected_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let task =
            query_task(&transaction, &workspace_root, name)?.ok_or(StoreError::TaskNotFound)?;
        transaction
            .execute(
                "
                INSERT INTO task_selections (workspace_root, task_id, selected_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_root) DO UPDATE SET
                    task_id = excluded.task_id,
                    selected_at = excluded.selected_at
                ",
                params![workspace_root, task.id, selected_at],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(task)
    }

    /// Returns the task currently selected for a workspace, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when stored task data cannot be read.
    pub fn selected_task(&self, workspace_root: impl AsRef<Path>) -> Result<Option<TaskRecord>> {
        let workspace_root = workspace_root_to_string(workspace_root.as_ref())?;
        let connection = self.connection.borrow();
        connection
            .query_row(
                "
                SELECT t.id, t.name, t.workspace_root, t.created_at
                FROM task_selections AS s
                INNER JOIN tasks AS t ON t.id = s.task_id
                WHERE s.workspace_root = ?1
                ",
                params![workspace_root],
                task_from_row,
            )
            .optional()
            .map_err(database_error)
    }

    /// Adds a new head for a task branch while preserving previous bindings.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or a failed transactional write.
    pub fn bind_session(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
        session: &SessionRef,
    ) -> Result<BindingRecord> {
        let branch_name = branch_name.as_ref();
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(session)?;

        let bound_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let id = replace_branch_head(&transaction, task_id, branch_name, session, bound_at)?;
        transaction.commit().map_err(database_error)?;

        Ok(BindingRecord {
            id,
            task_id,
            branch_name: branch_name.to_owned(),
            session: session.clone(),
            bound_at: timestamp_from_db(bound_at)?,
            is_current: true,
        })
    }

    /// Returns the active head of a task branch, if one exists.
    ///
    /// # Errors
    ///
    /// Returns an error when stored binding data cannot be read.
    pub fn current_binding(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
    ) -> Result<Option<BindingRecord>> {
        let connection = self.connection.borrow();
        connection
            .query_row(
                "
                SELECT id, task_id, branch_name, provider, session_id, bound_at, is_current
                FROM session_bindings
                WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
                ",
                params![task_id, branch_name.as_ref()],
                binding_from_row,
            )
            .optional()
            .map_err(database_error)
    }

    /// Records a provider-to-provider handoff and its JSON fidelity report.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references, invalid JSON, or persistence failure.
    pub fn record_handoff(
        &self,
        source: &SessionRef,
        target: &SessionRef,
        mode: TransferMode,
        fidelity: &Value,
    ) -> Result<()> {
        validate_session_ref(source)?;
        validate_session_ref(target)?;
        let fidelity_json =
            serde_json::to_string(fidelity).map_err(|_| StoreError::BundleEncoding)?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        insert_handoff(
            &transaction,
            source,
            target,
            mode,
            &fidelity_json,
            created_at,
        )?;
        transaction.commit().map_err(database_error)
    }

    /// Records a handoff and advances the task branch head in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or when either write cannot be committed.
    pub fn record_handoff_and_bind(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
        source: &SessionRef,
        target: &SessionRef,
        mode: TransferMode,
        fidelity: &Value,
    ) -> Result<BindingRecord> {
        let branch_name = branch_name.as_ref();
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(source)?;
        validate_session_ref(target)?;
        let fidelity_json =
            serde_json::to_string(fidelity).map_err(|_| StoreError::BundleEncoding)?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        insert_handoff(
            &transaction,
            source,
            target,
            mode,
            &fidelity_json,
            created_at,
        )?;
        let id = replace_branch_head(&transaction, task_id, branch_name, target, created_at)?;
        transaction.commit().map_err(database_error)?;

        Ok(BindingRecord {
            id,
            task_id,
            branch_name: branch_name.to_owned(),
            session: target.clone(),
            bound_at: timestamp_from_db(created_at)?,
            is_current: true,
        })
    }

    /// Records a handoff from `head` and makes `target` the head of its task branch, only while
    /// `head` is still that head, in one immediate transaction.
    ///
    /// Commands that read a head, work for a while, and then advance it use this, so a head that
    /// a concurrent command moved in the meantime is never replaced. When the head moved, nothing
    /// changes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, unreadable bindings, or failed persistence.
    pub fn record_handoff_and_advance_head(
        &self,
        head: &BindingRecord,
        target: &SessionRef,
        mode: TransferMode,
        fidelity: &Value,
    ) -> Result<BranchHeadAdvance> {
        if head.branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(&head.session)?;
        validate_session_ref(target)?;
        let fidelity_json =
            serde_json::to_string(fidelity).map_err(|_| StoreError::BundleEncoding)?;
        let created_at = now_timestamp();
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let current = transaction
            .query_row(
                "
                SELECT id FROM session_bindings
                WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
                ",
                params![head.task_id, head.branch_name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)?;
        if current != Some(head.id) {
            return Ok(BranchHeadAdvance::Moved);
        }
        insert_handoff(
            &transaction,
            &head.session,
            target,
            mode,
            &fidelity_json,
            created_at,
        )?;
        let id = replace_branch_head(
            &transaction,
            head.task_id,
            &head.branch_name,
            target,
            created_at,
        )?;
        transaction.commit().map_err(database_error)?;

        Ok(BranchHeadAdvance::Advanced(BindingRecord {
            id,
            task_id: head.task_id,
            branch_name: head.branch_name.clone(),
            session: target.clone(),
            bound_at: timestamp_from_db(created_at)?,
            is_current: true,
        }))
    }

    /// Returns handoffs in source-to-target creation order for lineage displays.
    ///
    /// # Errors
    ///
    /// Returns an error when stored handoff metadata cannot be read.
    pub fn handoff_lineage(&self) -> Result<Vec<HandoffRecord>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT source_provider, source_session_id, target_provider, target_session_id,
                       mode, created_at
                FROM handoffs
                ORDER BY created_at, id
                ",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], handoff_from_row)
            .map_err(database_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Stores a complete portable bundle, replacing a prior copy with its UUID.
    ///
    /// # Errors
    ///
    /// Returns an error when bundle serialization or persistence fails.
    pub fn save_bundle(&self, bundle: &PortableBundle) -> Result<()> {
        let bundle_json = serde_json::to_string(bundle).map_err(|_| StoreError::BundleEncoding)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "
                INSERT INTO bundles (bundle_id, bundle_json, saved_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (bundle_id) DO UPDATE SET
                    bundle_json = excluded.bundle_json,
                    saved_at = excluded.saved_at
                ",
                params![
                    bundle.manifest.bundle_id.to_string(),
                    bundle_json,
                    now_timestamp()
                ],
            )
            .map_err(database_error)?;
        protect_bundle_trajectory(&transaction, &bundle.snapshot.session)?;
        transaction.commit().map_err(database_error)
    }

    /// Stores a new bundle without replacing an existing UUID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::BundleAlreadyExists`] when UUID already exists.
    pub fn save_new_bundle(&self, bundle: &PortableBundle) -> Result<()> {
        let bundle_json = serde_json::to_string(bundle).map_err(|_| StoreError::BundleEncoding)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction
            .execute(
                "INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (?1, ?2, ?3)",
                params![
                    bundle.manifest.bundle_id.to_string(),
                    bundle_json,
                    now_timestamp()
                ],
            )
            .map_err(|error| {
                if matches!(
                    error,
                    rusqlite::Error::SqliteFailure(ref failure, _)
                        if failure.code == rusqlite::ErrorCode::ConstraintViolation
                ) {
                    StoreError::BundleAlreadyExists
                } else {
                    database_error(error)
                }
            })?;
        protect_bundle_trajectory(&transaction, &bundle.snapshot.session)?;
        transaction.commit().map_err(database_error)
    }

    /// Loads a portable bundle by UUID, if it has been stored.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored bundle cannot be read or decoded.
    pub fn load_bundle(&self, bundle_id: Uuid) -> Result<Option<PortableBundle>> {
        let connection = self.connection.borrow();
        let bundle_json = connection
            .query_row(
                "SELECT bundle_json FROM bundles WHERE bundle_id = ?1",
                params![bundle_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(database_error)?;
        bundle_json
            .map(|json| serde_json::from_str(&json).map_err(|_| StoreError::CorruptStore))
            .transpose()
    }

    /// Returns imported locator IDs for bundles from this provider, using the source index.
    ///
    /// # Errors
    ///
    /// Returns an error if the source index cannot be read.
    pub fn bundle_ids_for_source_provider(&self, provider: Provider) -> Result<Vec<String>> {
        let names =
            serde_json::to_string(provider.names()).map_err(|_| StoreError::BundleEncoding)?;
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare_cached(&format!(
                "SELECT bundle_id FROM bundles
                 WHERE {BUNDLE_SOURCE_PROVIDER_SQL} IN (SELECT value FROM json_each(?1))"
            ))
            .map_err(database_error)?;
        statement
            .query_map([names], |row| row.get(0))
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Looks up each imported locator's original provider without reading its transcript.
    ///
    /// # Errors
    ///
    /// Returns an error if the source index cannot be read.
    pub fn bundle_source_providers(&self) -> Result<std::collections::HashMap<String, Provider>> {
        let connection = self.connection.borrow();
        let mut query = connection
            .prepare(&format!(
                "SELECT bundle_id, {BUNDLE_SOURCE_PROVIDER_SQL} FROM bundles INDEXED BY bundle_source_session"
            ))
            .map_err(database_error)?;
        let mut rows = query.query([]).map_err(database_error)?;
        let mut providers = std::collections::HashMap::new();
        while let Some(row) = rows.next().map_err(database_error)? {
            if let Ok(name) = row.get::<_, String>(1)
                && let Ok(provider) = name.parse()
            {
                providers.insert(row.get(0).map_err(database_error)?, provider);
            }
        }
        Ok(providers)
    }

    /// Returns bundle UUIDs whose durable imported locator lacks metadata or search content.
    ///
    /// # Errors
    ///
    /// Returns an error when bundle index state cannot be read. Malformed IDs are skipped.
    pub fn bundle_ids_missing_imported_source(&self) -> Result<Vec<Uuid>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT bundles.bundle_id
                FROM bundles
                WHERE NOT EXISTS (
                    SELECT 1 FROM session_index
                    WHERE provider = 'imported'
                      AND session_id = bundles.bundle_id
                ) OR NOT EXISTS (
                    SELECT 1 FROM session_trajectories
                    WHERE provider = 'imported'
                      AND session_id = bundles.bundle_id
                )
                ORDER BY bundles.bundle_id
                ",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(database_error)?;
        let values = rows
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(|_| StoreError::CorruptStore)?;
        Ok(values
            .into_iter()
            .filter_map(|value| {
                Uuid::parse_str(&value)
                    .ok()
                    .filter(|bundle_id| bundle_id.to_string() == value)
            })
            .collect())
    }

    /// Returns cached native-session metadata ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions(&self) -> Result<Vec<IndexedSession>> {
        Self::read_indexed_sessions(&self.connection.borrow(), None, true)
    }

    /// Returns cached metadata for one provider, ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions_for_provider(&self, provider: Provider) -> Result<Vec<IndexedSession>> {
        Self::read_indexed_sessions(&self.connection.borrow(), Some(provider), true)
    }

    /// Whether a newly discovered native file can retry a hidden catalog row's earlier failure.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog or its recorded failure cannot be read.
    pub fn can_retry_hidden_session(
        &self,
        session: &SessionRef,
        observed: SourceRevision,
        document_version: u32,
    ) -> Result<bool> {
        let (Some(file), Some(contents)) = (observed.file, observed.contents) else {
            return Ok(false);
        };
        let connection = self.connection.borrow();
        let mut statement = connection.prepare_cached(
            "SELECT EXISTS (
                SELECT 1 FROM session_index AS catalog
                JOIN trajectory_index_failures AS failed USING (provider, session_id)
                WHERE catalog.provider = ?1 AND catalog.session_id = ?2
                  AND failed.source_file_id IS NOT NULL AND failed.source_contents_id IS NOT NULL
                  AND (failed.document_version < ?5
                       OR (failed.source_file_id, failed.source_contents_id) IS NOT (?3, ?4))
                  AND ((catalog.source_file_id, catalog.source_contents_id) IS (?3, ?4)
                       OR (catalog.source_file_id IS NULL AND EXISTS (
                            SELECT 1 FROM session_trajectories AS doc
                            WHERE doc.provider = catalog.provider AND doc.session_id = catalog.session_id
                              AND doc.origin = 'native' AND doc.source_complete AND doc.source_file_id IS NULL))))"
        ).map_err(database_error)?;
        statement
            .query_row(
                params![
                    session.provider.to_string(),
                    session.id,
                    file.into_bytes(),
                    contents.into_bytes(),
                    document_version
                ],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    fn read_indexed_sessions(
        connection: &Connection,
        provider: Option<Provider>,
        visible_only: bool,
    ) -> Result<Vec<IndexedSession>> {
        let mut sql = String::from(
            "SELECT provider, session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count, source_updated_submillis,
                    source_file_id, source_contents_id
             FROM session_index AS catalog WHERE 1",
        );
        if provider.is_some() {
            sql.push_str(" AND catalog.provider = ?1");
        }
        if visible_only {
            sql.push_str(
                " AND (catalog.provider NOT IN ('pi','omp') OR NOT EXISTS (
                    SELECT 1 FROM trajectory_index_failures AS failed
                    WHERE failed.provider = catalog.provider AND failed.session_id = catalog.session_id
                      AND failed.source_file_id IS NOT NULL AND failed.source_contents_id IS NOT NULL
                      AND (failed.source_file_id IS NOT catalog.source_file_id
                           OR failed.source_contents_id IS NOT catalog.source_contents_id)))",
            );
        }
        sql.push_str(" ORDER BY updated_at IS NULL, updated_at DESC, provider, session_id");
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        let rows = statement
            .query_map(
                rusqlite::params_from_iter(provider.map(|p| p.to_string())),
                indexed_session_from_row,
            )
            .map_err(database_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Inserts or refreshes one cached session without replacing other rows for its provider.
    ///
    /// This is used for durable local sources, whose lifecycle is independent from native
    /// provider discovery refreshes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or failed persistence.
    pub fn upsert_indexed_session(&self, session: &IndexedSession) -> Result<()> {
        Self::write_indexed_session(&self.connection.borrow(), session)
    }

    fn write_indexed_session(connection: &Connection, session: &IndexedSession) -> Result<()> {
        validate_session_ref(&session.session)?;
        let mut upsert = prepare_indexed_session(connection)?;
        write_indexed_session_row(&mut upsert, session, now_timestamp(), false)
    }

    /// Removes cached metadata and redacted search content for one native session.
    ///
    /// Current routing bindings to the session are deactivated. Native provider data,
    /// historical bindings, and handoff provenance are not changed.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or failed persistence.
    pub fn forget_session(&self, session: &SessionRef) -> Result<()> {
        validate_session_ref(session)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let provider = session.provider.to_string();
        transaction
            .execute(
                "DELETE FROM session_index WHERE provider = ?1 AND session_id = ?2",
                params![provider, session.id],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "DELETE FROM session_trajectories WHERE provider = ?1 AND session_id = ?2",
                params![provider, session.id],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "UPDATE session_bindings SET is_current = 0
                 WHERE provider = ?1 AND session_id = ?2 AND is_current = 1",
                params![provider, session.id],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)
    }

    /// Undoes the task branch head bound to a rolled-back session in one immediate transaction.
    ///
    /// When `session` is still the head, forgets it as [`Store::forget_session`] does and makes
    /// the binding it replaced the head again. When another binding moved the head first, nothing
    /// changes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, unreadable bindings, or failed persistence.
    pub fn restore_replaced_branch_head(
        &self,
        task_id: i64,
        branch_name: impl AsRef<str>,
        session: &SessionRef,
    ) -> Result<BranchHeadRestore> {
        let branch_name = branch_name.as_ref();
        if branch_name.trim().is_empty() {
            return Err(StoreError::InvalidBranchName);
        }
        validate_session_ref(session)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let head = transaction
            .query_row(
                "
                SELECT id, task_id, branch_name, provider, session_id, bound_at, is_current
                FROM session_bindings
                WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
                ",
                params![task_id, branch_name],
                binding_from_row,
            )
            .optional()
            .map_err(database_error)?;
        let Some(head) = head.filter(|head| head.session == *session) else {
            return Ok(BranchHeadRestore::Moved);
        };
        let replaced = transaction
            .query_row(
                "
                SELECT id, task_id, branch_name, provider, session_id, bound_at, is_current
                FROM session_bindings
                WHERE task_id = ?1 AND branch_name = ?2 AND id < ?3
                ORDER BY id DESC
                LIMIT 1
                ",
                params![task_id, branch_name, head.id],
                binding_from_row,
            )
            .optional()
            .map_err(database_error)?
            .map(|binding| binding.session);
        let provider = session.provider.to_string();
        for statement in [
            "DELETE FROM session_index WHERE provider = ?1 AND session_id = ?2",
            "DELETE FROM session_trajectories WHERE provider = ?1 AND session_id = ?2",
            "UPDATE session_bindings SET is_current = 0
             WHERE provider = ?1 AND session_id = ?2 AND is_current = 1",
        ] {
            transaction
                .execute(statement, params![provider, session.id])
                .map_err(database_error)?;
        }
        if let Some(replaced) = &replaced {
            replace_branch_head(
                &transaction,
                task_id,
                branch_name,
                replaced,
                now_timestamp(),
            )?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(BranchHeadRestore::Restored(replaced))
    }

    /// Returns when one provider's cached metadata was last refreshed.
    ///
    /// Empty provider snapshots retain refresh state, allowing repeated discovery
    /// to avoid rescanning stores that still contain no sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when cached refresh metadata cannot be read.
    pub fn session_index_refreshed_at(&self, provider: Provider) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connection.borrow();
        let timestamp = connection
            .query_row(
                "SELECT indexed_at FROM session_index_state WHERE provider = ?1",
                params![provider.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)?;
        timestamp.map(timestamp_from_db).transpose()
    }

    /// Returns when one provider store was last checked, including failed checks.
    ///
    /// # Errors
    ///
    /// Returns an error when cached check metadata cannot be read.
    pub fn session_index_checked_at(&self, provider: Provider) -> Result<Option<DateTime<Utc>>> {
        let connection = self.connection.borrow();
        let timestamp = connection
            .query_row(
                "SELECT checked_at FROM session_index_checks WHERE provider = ?1",
                params![provider.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)?;
        timestamp.map(timestamp_from_db).transpose()
    }

    /// Records a provider-store check without replacing its last valid snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when check metadata cannot be persisted.
    pub fn mark_session_index_checked(&self, provider: Provider) -> Result<()> {
        let connection = self.connection.borrow_mut();
        connection
            .execute(
                "
                INSERT INTO session_index_checks (provider, checked_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at
                ",
                params![provider.to_string(), now_timestamp()],
            )
            .map_err(database_error)?;
        Ok(())
    }

    /// Atomically replaces cached metadata for one provider.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched session providers or failed persistence.
    pub fn replace_indexed_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
    ) -> Result<()> {
        self.refresh_indexed_sessions(provider, sessions, true, None, |_, _| true)
    }

    /// Atomically merges observed metadata without pruning unobserved sessions or search history.
    ///
    /// Records the check time without claiming a complete provider refresh.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched session providers or failed persistence.
    pub fn merge_indexed_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
    ) -> Result<()> {
        self.refresh_indexed_sessions(provider, sessions, false, None, |_, _| true)
    }

    /// Captures the catalog generation for a provider whose native files have physical revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn observe_session_catalog(
        &self,
        provider: Provider,
    ) -> Result<Option<CatalogObservation>> {
        if !TrajectorySourceGuard::uses_file_time(provider) {
            return Ok(None);
        }
        Ok(Some(CatalogObservation {
            provider,
            generation: catalog_generation(&self.connection.borrow(), provider)?,
        }))
    }

    /// Captures individual catalog rows before rereading a provider whose listing changed.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid sessions or an unreadable catalog.
    pub fn observe_catalog_retry(
        &self,
        previous: CatalogObservation,
        listed: &[IndexedSession],
    ) -> Result<CatalogRetry> {
        let provider = previous.provider;
        validate_catalog_rows(provider, listed)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = connection.transaction().map_err(database_error)?;
        let observation = CatalogObservation {
            provider,
            generation: catalog_generation(&transaction, provider)?,
        };
        let rows = catalog_rows_by_id(&transaction, provider)?;
        transaction.commit().map_err(database_error)?;
        let listed = catalog_id_counts(listed)
            .into_iter()
            .map(|(id, count)| (id.to_owned(), count))
            .collect();
        Ok(CatalogRetry {
            observation,
            rows,
            listed,
        })
    }

    /// Publishes a repeated listing or merges the rows whose catalog state and files still agree.
    /// A partial merge returns accepted positions and preserves provider refresh times and unlisted rows.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid sessions or a failed database operation.
    pub fn refresh_retried_indexed_sessions(
        &self,
        retry: &CatalogRetry,
        sessions: &[IndexedSession],
        complete: bool,
        verify: impl Fn(usize, SourceRevision) -> bool,
    ) -> Result<Option<Vec<usize>>> {
        let provider = retry.observation.provider;
        validate_catalog_rows(provider, sessions)?;
        let counts = catalog_id_counts(sessions);
        let previous_ids_accounted_for = retry
            .listed
            .iter()
            .all(|(id, count)| *count == 1 && (complete || counts.contains_key(id.as_str())));
        if previous_ids_accounted_for && counts.values().all(|&count| count == 1) {
            match self.refresh_observed_indexed_sessions(
                retry.observation,
                sessions,
                complete,
                &verify,
            ) {
                Ok(()) => return Ok(None),
                Err(StoreError::TrajectorySourceChanged) => {}
                Err(error) => return Err(error),
            }
        }
        self.merge_retried_indexed_sessions(retry, sessions, verify)
            .map(Some)
    }

    /// Publishes only rows that still agree with the retried inventory, preserving refresh times.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid sessions or a failed database operation.
    pub fn merge_retried_indexed_sessions(
        &self,
        retry: &CatalogRetry,
        sessions: &[IndexedSession],
        verify: impl Fn(usize, SourceRevision) -> bool,
    ) -> Result<Vec<usize>> {
        let provider = retry.observation.provider;
        validate_catalog_rows(provider, sessions)?;
        let counts = catalog_id_counts(sessions);
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let current = (catalog_generation(&transaction, provider)? != retry.observation.generation)
            .then(|| catalog_rows_by_id(&transaction, provider))
            .transpose()?;
        let mut accepted = Vec::new();
        let indexed_at = now_timestamp();
        {
            let mut upsert = prepare_indexed_session(&transaction)?;
            for (index, row) in sessions.iter().enumerate() {
                let id = row.session.id.as_str();
                if counts[id] != 1
                    || retry.listed.get(id).is_some_and(|&count| count > 1)
                    || current
                        .as_ref()
                        .is_some_and(|rows| rows.get(id) != retry.rows.get(id))
                    || !row
                        .source_revision
                        .is_some_and(|revision| revision.file.is_some() && verify(index, revision))
                {
                    continue;
                }
                write_indexed_session_row(&mut upsert, row, indexed_at, false)?;
                accepted.push(index);
            }
        }
        transaction.commit().map_err(database_error)?;
        Ok(accepted)
    }

    /// Publishes the catalog only if both the store and the listed files still match discovery.
    /// The verifier receives the position and file revision of each tracked session.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog or a selected file changed, or a write fails.
    pub fn refresh_observed_indexed_sessions(
        &self,
        observation: CatalogObservation,
        sessions: &[IndexedSession],
        complete: bool,
        verify: impl Fn(usize, SourceRevision) -> bool,
    ) -> Result<()> {
        self.refresh_indexed_sessions(
            observation.provider,
            sessions,
            complete,
            Some(observation),
            verify,
        )
    }

    fn refresh_indexed_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
        complete: bool,
        observation: Option<CatalogObservation>,
        verify: impl Fn(usize, SourceRevision) -> bool,
    ) -> Result<()> {
        validate_catalog_rows(provider, sessions)?;
        if complete || observation.is_some() {
            let mut listed = HashSet::with_capacity(sessions.len());
            if sessions
                .iter()
                .any(|entry| !listed.insert(&entry.session.id))
            {
                return Err(if observation.is_some() {
                    StoreError::TrajectorySourceChanged
                } else {
                    StoreError::InvalidSessionReference
                });
            }
        }
        let mut connection = self.connection.borrow_mut();
        let unchanged = complete && provider_index_matches(&connection, provider, sessions)?;
        let transaction = immediate_transaction(&mut connection)?;
        if let Some(prior) = observation
            && prior.generation != catalog_generation(&transaction, provider)?
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let provider_name = provider.to_string();
        let indexed_at = now_timestamp();
        if !unchanged {
            write_discovered_sessions(&transaction, provider, sessions, complete, indexed_at)?;
        }
        transaction
            .execute(
                "
                INSERT INTO session_index_checks (provider, checked_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at
                ",
                params![provider_name, indexed_at],
            )
            .map_err(database_error)?;
        if complete {
            prune_stale_native_trajectories(&transaction, &provider_name)?;
        }
        if observation.is_some()
            && sessions.iter().enumerate().any(|(index, entry)| {
                entry
                    .source_revision
                    .is_some_and(|revision| revision.file.is_some() && !verify(index, revision))
            })
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction.commit().map_err(database_error)
    }

    /// Stores one redacted session trajectory for full-text search.
    ///
    /// Existing content for the same native session is replaced atomically when source state is
    /// newer. Equal source state prefers complete-source reads, then complete coverage. Callers
    /// must redact secrets and omit hidden reasoning before passing `redacted_text`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or failed persistence.
    pub fn upsert_session_trajectory(
        &self,
        session: &SessionRef,
        redacted_text: &str,
        source_updated_at: DateTime<Utc>,
        complete: bool,
    ) -> Result<()> {
        let byte_count = redacted_text.len();
        self.upsert_session_trajectory_document(
            session,
            redacted_text,
            source_updated_at,
            byte_count,
            byte_count,
            if complete { "none" } else { "legacy_bounded" },
            complete,
            SessionTrajectoryOrigin::Native,
        )
    }

    /// Stores one redacted search document with explicit coverage and provenance metadata.
    ///
    /// Completeness is derived from counts and strategy. Bounded or source-incomplete content
    /// cannot be persisted as complete. Bundle presence protects a row from provider-discovery
    /// pruning without preventing newer native content from replacing its indexed text.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid coverage, session references, or failed persistence.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session_trajectory_document(
        &self,
        session: &SessionRef,
        redacted_text: &str,
        source_updated_at: DateTime<Utc>,
        source_byte_count: usize,
        indexed_byte_count: usize,
        truncation_strategy: &str,
        source_complete: bool,
        origin: SessionTrajectoryOrigin,
    ) -> Result<()> {
        self.upsert_trajectory_document(
            session,
            &TrajectoryDocument {
                source_revision: SourceRevision::default(),
                redacted_text,
                source_updated_at,
                source_byte_count,
                indexed_byte_count,
                truncation_strategy,
                source_complete,
                provider_limited: false,
                origin,
                document_version: 0,
                derived_title: None,
                title_authoritative: false,
            },
        )
    }

    /// Stores one versioned search document.
    ///
    /// A preview cannot replace a full document during a version upgrade unless the provider
    /// reached its full-read limit. Older document versions cannot replace newer versions.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid coverage, session references, or failed persistence.
    pub fn upsert_trajectory_document(
        &self,
        session: &SessionRef,
        document: &TrajectoryDocument<'_>,
    ) -> Result<()> {
        self.upsert_trajectory_documents_with_limits(
            std::iter::once((session, *document, TrajectorySourceGuard::NewerSource, None)),
            usize::MAX,
            |_, _| true,
        )
        .map(|_| ())
    }

    /// Stores a bounded set of redacted search documents in one atomic transaction.
    ///
    /// Documents are applied in slice order, retaining the same version, source-coverage,
    /// provenance, derived-title, and bundle-protection behavior as individual upserts. Every
    /// document is validated before the transaction starts. Any database failure rolls back the
    /// whole batch. Empty slices are no-ops. One batch can contain at most
    /// [`MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS`] documents and
    /// [`MAX_TRAJECTORY_WRITE_BATCH_BYTES`] combined indexed text bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid document or session reference, a batch over either safety
    /// bound, or failed persistence.
    pub fn upsert_trajectory_documents(
        &self,
        documents: &[(&SessionRef, TrajectoryDocument<'_>)],
    ) -> Result<()> {
        self.upsert_trajectory_documents_with_limits(
            documents.iter().map(|&(session, document)| {
                (session, document, TrajectorySourceGuard::NewerSource, None)
            }),
            MAX_TRAJECTORY_WRITE_BATCH_BYTES,
            |_, _| true,
        )
        .map(|_| ())
    }

    /// Stores a batch with each reader's source observation. A single document may exceed the
    /// combined batch byte limit, as with [`Self::upsert_trajectory_document`].
    ///
    /// # Errors
    ///
    /// Returns an error if a document is invalid, the batch exceeds its limits, or a write fails.
    pub fn upsert_observed_trajectory_documents(
        &self,
        documents: &[(&SessionRef, TrajectoryDocument<'_>, TrajectorySourceGuard)],
    ) -> Result<()> {
        self.upsert_observed_trajectory_documents_counted(documents)
            .map(|_| ())
    }

    /// Stores an observed batch and returns the number of distinct search documents rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error if a document is invalid, the batch exceeds its limits, or a write fails.
    pub fn upsert_observed_trajectory_documents_counted(
        &self,
        documents: &[(&SessionRef, TrajectoryDocument<'_>, TrajectorySourceGuard)],
    ) -> Result<usize> {
        self.upsert_observed_trajectory_documents_checked(documents, |_, _| true)
    }

    /// Verifies the selected native files while holding the write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if a selected file changed, the batch is invalid, or a write fails.
    pub fn upsert_observed_trajectory_documents_checked(
        &self,
        documents: &[(&SessionRef, TrajectoryDocument<'_>, TrajectorySourceGuard)],
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_selected_trajectory_documents_checked(
            documents
                .iter()
                .map(|&(session, document, guard)| (session, document, guard, None)),
            verify,
        )
    }

    /// Publishes a batch and any catalog metadata captured by its readers in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if a catalog does not match its document, a selected file changed, or a write failed.
    pub fn upsert_selected_trajectory_documents_checked<'s, 'd>(
        &self,
        documents: impl ExactSizeIterator<Item = SelectedTrajectoryDocument<'s, 'd>>,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        let max_indexed_bytes = if documents.len() == 1 {
            usize::MAX
        } else {
            MAX_TRAJECTORY_WRITE_BATCH_BYTES
        };
        self.upsert_trajectory_documents_with_limits(documents, max_indexed_bytes, verify)
    }

    /// Captures the current search document before reading a source whose update time can repeat.
    ///
    /// # Errors
    ///
    /// Returns an error when the reference is invalid or the document cannot be read.
    pub fn observe_trajectory_document(
        &self,
        session: &SessionRef,
    ) -> Result<TrajectorySourceGuard> {
        validate_session_ref(session)?;
        document_receipt(&self.connection.borrow(), session)
            .map(TrajectorySourceGuard::ObservedDocument)
    }

    /// Captures the current search document and catalog before reading the selected session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session reference is invalid or the store cannot read its state.
    pub fn observe_verified_session(&self, session: &SessionRef) -> Result<TrajectorySourceGuard> {
        validate_session_ref(session)?;
        verified_session_state(&self.connection.borrow(), session)
    }

    /// Captures the session state and checks whether its stored sources name a different file.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or an unreadable store.
    pub fn observe_index_file(
        &self,
        session: &SessionRef,
        file: Uuid,
    ) -> Result<(TrajectorySourceGuard, bool)> {
        validate_session_ref(session)?;
        observed_file_state(&self.connection.borrow(), session, Some(file))
    }

    /// Publishes the native catalog and search document from the same selected source.
    /// Returns the number of search documents rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error if a preview lacks a file revision, the observed state changed, or a write failed.
    pub fn upsert_verified_session_document(
        &self,
        metadata: &IndexedSession,
        document: TrajectoryDocument<'_>,
        guard: TrajectorySourceGuard,
    ) -> Result<usize> {
        self.upsert_verified_session_document_checked(metadata, document, guard, |_, _| true)
    }

    /// Publishes the selected catalog and document after checking the native file inside the write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected file or the observed store state changed, or a write fails.
    pub fn upsert_verified_session_document_checked(
        &self,
        metadata: &IndexedSession,
        document: TrajectoryDocument<'_>,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_selected_trajectory_documents_checked(
            std::iter::once((&metadata.session, document, guard, Some(metadata))),
            verify,
        )
    }

    fn upsert_trajectory_documents_with_limits<'s, 'd>(
        &self,
        documents: impl ExactSizeIterator<Item = SelectedTrajectoryDocument<'s, 'd>>,
        max_indexed_bytes: usize,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        if documents.len() == 0 {
            return Ok(0);
        }
        if documents.len() > MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }

        let mut remaining_bytes = max_indexed_bytes;
        let mut prepared = Vec::with_capacity(documents.len());
        for selected in documents {
            let write = PreparedTrajectoryWrite::new(selected, remaining_bytes)?;
            remaining_bytes -= write.document.indexed_byte_count;
            prepared.push(write);
        }

        self.publish_trajectory_documents(&prepared, verify)
    }

    fn publish_trajectory_documents(
        &self,
        prepared: &[PreparedTrajectoryWrite<'_, '_>],
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let mut rewritten: Vec<(i64, &str)> = Vec::with_capacity(prepared.len());
        let mut upsert_parent = transaction
            .prepare_cached(UPSERT_TRAJECTORY_PARENT_SQL)
            .map_err(database_error)?;
        for write in prepared {
            if !write.should_publish(&transaction)? {
                continue;
            }
            let verified_metadata = write.verified_metadata;
            let revision = write.document.source_revision;
            let jsonl_preview = !write.source_complete
                && write.document.origin == SessionTrajectoryOrigin::Native
                && samples_jsonl_tail(write.session.provider);
            let trajectory_id = upsert_parent
                .query_row(
                    params![
                        write.session.provider.to_string(),
                        write.session.id,
                        write.content_hash,
                        write.document.source_updated_at.timestamp_millis(),
                        i64::from(write.source_complete),
                        i64::from(write.complete),
                        now_timestamp(),
                        write.source_byte_count,
                        write.indexed_byte_count,
                        write.document.truncation_strategy,
                        write.document.origin.as_str(),
                        i64::from(write.document.origin == SessionTrajectoryOrigin::ImportedBundle),
                        i64::from(write.document.document_version),
                        write.document.derived_title,
                        write.guard != TrajectorySourceGuard::NewerSource,
                        source_submillis(write.session.provider, write.document.source_updated_at),
                        write.document.provider_limited,
                        write.document.can_clear_derived_title(),
                        (verified_metadata.is_some()
                            && (write.source_complete || write.document.provider_limited))
                            || write.observed_full_source(),
                        revision.file.map(Uuid::into_bytes),
                        revision.missing_ancestor.map(Uuid::into_bytes),
                        revision.contents.map(Uuid::into_bytes),
                        verified_metadata.is_some(),
                        jsonl_preview,
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(database_error)?;
            if let Some(metadata) = verified_metadata {
                if trajectory_id.is_none() && !write.matches_stored(&transaction)? {
                    if !write.source_complete && !write.document.provider_limited {
                        // A sample can confirm the header while an earlier full read still covers more history.
                        // Keep that document with its original catalog until a full read can update both.
                        continue;
                    }
                    return Err(StoreError::TrajectorySourceChanged);
                }
                if !matches_catalog_row(&transaction, metadata)? {
                    let mut upsert = prepare_indexed_session(&transaction)?;
                    write_indexed_session_row(&mut upsert, metadata, now_timestamp(), true)?;
                }
                if write.document.source_revision.file.is_some() {
                    transaction.execute(
                        "DELETE FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2",
                        params![write.session.provider.to_string(), write.session.id],
                    ).map_err(database_error)?;
                }
            }
            if let Some(trajectory_id) = trajectory_id {
                if jsonl_preview {
                    transaction
                        .execute(
                            "UPDATE session_trajectories SET tail_reader_version = 1 WHERE id = ?1",
                            [trajectory_id],
                        )
                        .map_err(database_error)?;
                }
                // A later write of the same session replaces an earlier one in this batch.
                rewritten.retain(|(other, _)| *other != trajectory_id);
                rewritten.push((trajectory_id, write.document.redacted_text));
            } else if write.document.origin == SessionTrajectoryOrigin::ImportedBundle {
                transaction
                    .execute(
                        "UPDATE session_trajectories SET protected_by_bundle = 1
                         WHERE provider = ?1 AND session_id = ?2",
                        params![write.session.provider.to_string(), write.session.id],
                    )
                    .map_err(database_error)?;
            }
        }
        drop(upsert_parent);
        replace_trajectory_chunks(&transaction, &rewritten)?;
        for write in prepared {
            if write.document.origin == SessionTrajectoryOrigin::Native
                && write.document.source_revision.file.is_some()
                && !verify(write.session, write.document.source_revision)
            {
                return Err(StoreError::TrajectorySourceChanged);
            }
        }
        transaction.commit().map_err(database_error)?;
        Ok(rewritten.len())
    }

    /// Returns whether complete indexed content already covers source state.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references or failed persistence.
    pub fn session_trajectory_is_current(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow();
        connection
            .query_row(
                "
                SELECT EXISTS (
                    SELECT 1
                    FROM session_trajectories
                    WHERE provider = ?1
                      AND session_id = ?2
                      AND complete = 1
                      AND source_updated_at >= ?3 AND (NOT ?4 OR (source_updated_at = ?3 AND source_updated_submillis = ?5))
                )
                ",
                params![
                    session.provider.to_string(),
                    session.id,
                    source_updated_at.timestamp_millis(),
                    TrajectorySourceGuard::uses_file_time(session.provider),
                    source_submillis(session.provider, source_updated_at),
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)
    }

    /// Returns whether any safely bounded indexed document covers source state.
    ///
    /// Unlike [`Self::session_trajectory_is_current`], this accepts truthful head-tail coverage
    /// and prevents repeated indexing of an unchanged oversized source.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references or failed persistence.
    pub fn session_trajectory_source_is_current(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.session_trajectory_source_is_current_at_version(session, source_updated_at, 0)
    }

    /// Checks whether a complete source read used at least the requested search document version.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references or failed persistence.
    pub fn session_trajectory_source_is_current_at_version(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
        document_version: u32,
    ) -> Result<bool> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow();
        connection
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM session_trajectories
                    WHERE provider = ?1 AND session_id = ?2
                      AND source_complete = 1 AND source_updated_at >= ?3 AND document_version >= ?4
                      AND (NOT ?5 OR (source_updated_at = ?3 AND source_updated_submillis = ?6))
                )",
                params![
                    session.provider.to_string(),
                    session.id,
                    source_updated_at.timestamp_millis(),
                    document_version,
                    TrajectorySourceGuard::uses_file_time(session.provider),
                    source_submillis(session.provider, source_updated_at),
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)
    }

    /// Reads one document's coverage, source timestamp, and version before opening its source.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or when the index cannot be read.
    pub fn trajectory_index_state(
        &self,
        session: &SessionRef,
    ) -> Result<Option<TrajectoryIndexState>> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow();
        connection.query_row(
            "SELECT source_updated_at, source_complete, document_version, source_updated_submillis,
                    provider_limited, title_authoritative, source_file_id, missing_ancestor_id, source_contents_id,
                    CASE WHEN origin = 'native' AND source_complete = 0 THEN tail_reader_version END
             FROM session_trajectories WHERE provider = ?1 AND session_id = ?2",
            params![session.provider.to_string(), session.id], |row| state_from_row(row, 0, session.provider),
        ).optional().map_err(database_error)
    }

    /// Returns the indexing state of every cached search document.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read or contains invalid data.
    pub fn trajectory_index_states(
        &self,
    ) -> Result<std::collections::HashMap<SessionRef, TrajectoryIndexState>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT provider, session_id, source_updated_at, source_complete, document_version,
                    source_updated_submillis, provider_limited, title_authoritative, source_file_id,
                    missing_ancestor_id, source_contents_id,
                    CASE WHEN origin = 'native' AND source_complete = 0 THEN tail_reader_version END FROM session_trajectories",
            )
            .map_err(database_error)?;
        let mut rows = statement.query([]).map_err(database_error)?;
        let mut states = std::collections::HashMap::new();
        while let Some(row) = rows.next().map_err(database_error)? {
            let provider = row
                .get::<_, String>(0)
                .map_err(database_error)?
                .parse::<Provider>()
                .map_err(|_| StoreError::CorruptStore)?;
            let session =
                SessionRef::new(provider, row.get::<_, String>(1).map_err(database_error)?);
            states.insert(
                session,
                state_from_row(row, 2, provider).map_err(database_error)?,
            );
        }
        Ok(states)
    }

    /// Reads the previous failure for one session.
    ///
    /// # Errors
    ///
    /// Returns an error when the failure cannot be read.
    pub fn trajectory_index_failure(
        &self,
        session: &SessionRef,
    ) -> Result<Option<TrajectoryIndexFailure>> {
        validate_session_ref(session)?;
        index_failure(&self.connection.borrow(), session)
    }

    /// Returns every recorded search indexing failure.
    ///
    /// # Errors
    ///
    /// Returns an error when failures cannot be read or contain invalid data.
    pub fn trajectory_index_failures(
        &self,
    ) -> Result<std::collections::HashMap<SessionRef, TrajectoryIndexFailure>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT provider, session_id, source_updated_at, document_version, source_updated_submillis,
                        source_file_id, source_contents_id FROM trajectory_index_failures",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    failure_from_row(row, 2)?,
                ))
            })
            .map_err(database_error)?;
        let mut failures = std::collections::HashMap::new();
        for row in rows {
            let (provider, session_id, failure) = row.map_err(|_| StoreError::CorruptStore)?;
            let provider = provider
                .parse::<Provider>()
                .map_err(|_| StoreError::CorruptStore)?;
            failures.insert(SessionRef::new(provider, session_id), failure);
        }
        Ok(failures)
    }

    /// Records that one session could not be read for indexing, replacing an earlier failure.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or failed persistence.
    pub fn record_trajectory_index_failure(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
    ) -> Result<()> {
        self.record_observed_trajectory_index_failure(
            session,
            failure,
            TrajectorySourceGuard::NewerSource,
        )
    }

    /// Records a read failure for the source the caller observed.
    ///
    /// # Errors
    ///
    /// Returns an error when the indexed source changed or the failure cannot be saved.
    pub fn record_observed_trajectory_index_failure(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
        guard: TrajectorySourceGuard,
    ) -> Result<()> {
        self.record_observed_trajectory_index_failure_checked(session, failure, guard, |_, _| true)
    }

    /// Verifies that the failed file is still selected before committing the failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the source changed or the failure cannot be saved.
    pub fn record_observed_trajectory_index_failure_checked(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<()> {
        validate_session_ref(session)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        guard.check(
            &transaction,
            session,
            failure.source_updated_at.unwrap_or_default(),
        )?;
        transaction.execute(
                "INSERT INTO trajectory_index_failures (
                     provider, session_id, source_updated_at, document_version, failed_at, source_updated_submillis, source_file_id, source_contents_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (provider, session_id) DO UPDATE SET
                     source_updated_at = excluded.source_updated_at,
                     source_updated_submillis = excluded.source_updated_submillis,
                     document_version = excluded.document_version,
                     failed_at = excluded.failed_at, source_file_id = excluded.source_file_id,
                     source_contents_id = excluded.source_contents_id",
                params![
                    session.provider.to_string(),
                    session.id,
                    failure
                        .source_updated_at
                        .as_ref()
                        .map(DateTime::timestamp_millis),
                    i64::from(failure.document_version),
                    now_timestamp(),
                    failure.source_updated_at.filter(|_| failure.source_time_precise).map(|time| source_submillis(session.provider, time)),
                    failure.source_file.map(Uuid::into_bytes),
                    failure.source_contents.map(Uuid::into_bytes),
                ],
            )
            .map_err(database_error)?;
        let revision = SourceRevision {
            file: failure.source_file,
            contents: failure.source_contents,
            missing_ancestor: None,
        };
        if revision.file.is_some() && !verify(session, revision) {
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction.commit().map_err(database_error)
    }

    /// Clears a failure only if it still matches the source and format the caller saw.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session or a failed database write.
    pub fn clear_observed_trajectory_index_failure(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
    ) -> Result<()> {
        validate_session_ref(session)?;
        self.connection
            .borrow()
            .execute(
                "DELETE FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2
             AND source_updated_at IS ?3 AND document_version = ?4 AND source_file_id IS ?5
             AND source_contents_id IS ?6 AND source_updated_submillis IS ?7",
                params![
                    session.provider.to_string(),
                    session.id,
                    failure
                        .source_updated_at
                        .map(|time| time.timestamp_millis()),
                    failure.document_version,
                    failure.source_file.map(Uuid::into_bytes),
                    failure.source_contents.map(Uuid::into_bytes),
                    failure
                        .source_updated_at
                        .filter(|_| failure.source_time_precise)
                        .map(|time| source_submillis(session.provider, time))
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    /// Forgets the recorded search indexing failure of one session.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or failed persistence.
    pub fn clear_trajectory_index_failure(&self, session: &SessionRef) -> Result<()> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow_mut();
        connection
            .execute(
                "DELETE FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2",
                params![session.provider.to_string(), session.id],
            )
            .map_err(database_error)?;
        Ok(())
    }

    /// Returns a provider's native documents whose catalog selects a different file or contents.
    ///
    /// # Errors
    ///
    /// Returns an error if the index cannot be read.
    pub fn conflicting_file_catalog_rows(
        &self,
        provider: Provider,
    ) -> Result<std::collections::HashSet<SessionRef>> {
        let connection = self.connection.borrow();
        let sql = format!(
            "SELECT trajectories.session_id FROM session_trajectories AS trajectories
             INNER JOIN session_index AS catalog ON {CONFLICTING_FILE_CATALOG}
             WHERE trajectories.provider = ?1"
        );
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        statement
            .query_map([provider.to_string()], |row| {
                Ok(SessionRef::new(provider, row.get::<_, String>(0)?))
            })
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns the derived title that survived a search document update.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed read.
    pub fn trajectory_title(&self, session: &SessionRef) -> Result<Option<String>> {
        Ok(self
            .trajectory_title_records(Some(session))?
            .remove(session)
            .and_then(|record| record.title))
    }

    /// Returns redacted titles derived from indexed conversations.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read or contains invalid data.
    pub fn trajectory_titles(&self) -> Result<std::collections::HashMap<SessionRef, String>> {
        Ok(self
            .trajectory_title_records(None)?
            .into_iter()
            .filter_map(|(session, record)| Some((session, record.title?)))
            .collect())
    }

    /// Reads each title with the source time and version of the document that verified it.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or unreadable index.
    pub fn trajectory_title_records(
        &self,
        session: Option<&SessionRef>,
    ) -> Result<std::collections::HashMap<SessionRef, IndexedTitle>> {
        let mut sql = format!("SELECT provider, session_id, derived_title, document_version,
            source_updated_at, source_updated_submillis, coalesce(title_authoritative, source_complete),
            CASE WHEN trajectories.provider = 'omp' AND coalesce(trajectories.title_authoritative, trajectories.source_complete)
             THEN (SELECT coalesce(catalog.source_updated_submillis, 0)
                   FROM session_index AS catalog
                   WHERE catalog.provider = trajectories.provider AND catalog.session_id = trajectories.session_id
                     AND catalog.updated_at = trajectories.source_updated_at AND
                     (catalog.source_updated_submillis = trajectories.source_updated_submillis
                      OR (catalog.source_updated_submillis IS NULL AND trajectories.source_complete))) END,
            source_file_id, missing_ancestor_id, source_contents_id
            FROM session_trajectories AS trajectories
            WHERE NOT EXISTS (SELECT 1 FROM session_index AS catalog WHERE {CONFLICTING_FILE_CATALOG})
              AND NOT EXISTS (SELECT 1 FROM trajectory_index_failures AS failed WHERE {HIDDEN_FAILED_TRAJECTORY}) ");
        let scope = if let Some(session) = session {
            validate_session_ref(session)?;
            sql.push_str("AND provider = ?1 AND session_id = ?2");
            vec![session.provider.to_string(), session.id.clone()]
        } else {
            sql.push_str(
                "AND (derived_title IS NOT NULL OR (provider = 'omp' AND title_authoritative = 1))",
            );
            Vec::new()
        };
        let connection = self.connection.borrow();
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let mut rows = statement
            .query(rusqlite::params_from_iter(scope))
            .map_err(database_error)?;
        let mut titles = std::collections::HashMap::new();
        while let Some(row) = rows.next().map_err(database_error)? {
            let provider = row
                .get::<_, String>(0)
                .map_err(database_error)?
                .parse::<Provider>()
                .map_err(|_| StoreError::CorruptStore)?;
            let session_id: String = row.get(1).map_err(database_error)?;
            let remainder: Option<u32> = row.get(5).map_err(database_error)?;
            let millis = row.get(4).map_err(database_error)?;
            let verified_at = if provider == Provider::OhMyPi
                && remainder.is_some()
                && row.get::<_, bool>(6).map_err(database_error)?
            {
                Some((
                    row.get(3).map_err(database_error)?,
                    source_time_from_db(millis, remainder)?,
                ))
            } else {
                None
            };
            titles.insert(
                SessionRef::new(provider, session_id),
                IndexedTitle {
                    source_revision: source_revision_from_row(row, 8).map_err(database_error)?,
                    title: row.get(2).map_err(database_error)?,
                    verified_at,
                    cached_at: row
                        .get::<_, Option<u32>>(7)
                        .map_err(database_error)?
                        .map(|fraction| source_time_from_db(millis, Some(fraction)))
                        .transpose()?,
                },
            );
        }
        Ok(titles)
    }

    /// Finds session references whose redacted trajectories contain `query`.
    ///
    /// `query` parses with [`SearchQuery::parse`], and every term must match somewhere in the
    /// session. Plain words match as token prefixes. A word with inner punctuation, such as
    /// `qwen3.8`, matches its tokens as adjacent words, so `qwen3-8` matches but `qwen3` and `8`
    /// far apart do not. A quoted phrase prefilters on the same token phrase, then requires the
    /// matching chunk to contain the phrase itself, case-insensitively with spaces and punctuation
    /// included. Because the prefilter is token-based, a phrase's first word must start a word in
    /// the text. FTS operators and punctuation never change query structure. Results expose session
    /// references only, never indexed transcript content.
    ///
    /// # Errors
    ///
    /// Returns an error when the full-text index cannot be queried or contains invalid data.
    pub fn search_session_trajectories(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionRef>> {
        self.search_session_trajectory_matches(query, limit)
            .map(|matches| matches.into_iter().map(|item| item.session).collect())
    }

    /// Finds ranked session matches with bounded redacted context around matched terms.
    ///
    /// Returned snippets come only from content already admitted to local redacted search index.
    /// Hidden reasoning, approvals, secrets, and provider-only metadata never enter this table.
    ///
    /// # Errors
    ///
    /// Returns an error when full-text index cannot be queried or contains invalid data.
    pub fn search_session_trajectory_matches(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionTrajectoryMatch>> {
        self.search_session_trajectory_page(query, limit)
            .map(|page| page.matches)
    }

    /// Finds one bounded ranked page and reports whether additional matches exist.
    ///
    /// Delivery is capped even when callers request an excessive limit. Equal ranks use source
    /// state, provider, and session ID for stable deterministic ordering.
    ///
    /// # Errors
    ///
    /// Returns an error when full-text index cannot be queried or contains invalid data.
    pub fn search_session_trajectory_page(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<SessionTrajectorySearchPage> {
        self.search_session_trajectory_page_with_eligibility(query, limit, None)
    }

    /// Finds one bounded ranked page among explicitly eligible sessions.
    ///
    /// Filtering occurs before ranking and limiting, so stronger matches outside a picker scope or
    /// provider cannot displace eligible results.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session references, query failures, or invalid indexed data.
    pub fn search_session_trajectory_page_for_sessions(
        &self,
        query: &str,
        limit: usize,
        eligible_sessions: &[SessionRef],
    ) -> Result<SessionTrajectorySearchPage> {
        let selections = eligible_sessions
            .iter()
            .map(|session| TrajectorySelection {
                session: session.clone(),
                source: None,
            })
            .collect::<Vec<_>>();
        self.search_selected_trajectory_page(query, limit, &selections)
    }

    /// Ranks only documents whose revision matches the selected rollout in the query snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid references or an unreadable index.
    pub fn search_selected_trajectory_page(
        &self,
        query: &str,
        limit: usize,
        eligible_sessions: &[TrajectorySelection],
    ) -> Result<SessionTrajectorySearchPage> {
        for selection in eligible_sessions {
            validate_session_ref(&selection.session)?;
        }
        if eligible_sessions.is_empty() {
            return Ok(empty_trajectory_search_page());
        }
        let mut eligibility = eligible_sessions
            .iter()
            .filter(|selection| {
                selection
                    .source
                    .is_none_or(|revision| revision.file.is_some())
            })
            .map(|selection| {
                let encode = |id: Uuid| id.simple().to_string().to_ascii_uppercase();
                (
                    selection.session.provider.to_string(),
                    &selection.session.id,
                    selection
                        .source
                        .and_then(|revision| revision.file)
                        .map(encode),
                    selection
                        .source
                        .map(|revision| revision.contents.map(encode).unwrap_or_default()),
                )
            })
            .collect::<Vec<_>>();
        eligibility.sort_unstable();
        eligibility.dedup();
        let eligibility_json = serde_json::to_string(&eligibility)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        self.search_session_trajectory_page_with_eligibility(query, limit, Some(&eligibility_json))
    }

    fn search_session_trajectory_page_with_eligibility(
        &self,
        query: &str,
        limit: usize,
        eligibility_json: Option<&str>,
    ) -> Result<SessionTrajectorySearchPage> {
        let clauses = trajectory_clauses(query);
        if clauses.is_empty() || limit == 0 {
            return Ok(empty_trajectory_search_page());
        }
        let limit = limit.min(TRAJECTORY_SEARCH_RESULT_LIMIT);
        let query_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let statement_sql = trajectory_search_page_sql(&clauses);
        let mut parameters = clauses
            .iter()
            .map(|clause| SqlValue::Text(clause.match_expression.clone()))
            .collect::<Vec<_>>();
        parameters
            .push(eligibility_json.map_or(SqlValue::Null, |json| SqlValue::Text(json.to_owned())));
        parameters.push(SqlValue::Integer(query_limit));
        parameters.extend(
            clauses
                .iter()
                .filter_map(|clause| clause.exact_phrase.clone().map(SqlValue::Text)),
        );
        let connection = self.connection.borrow();
        // Ranked rows and their excerpts come from one read snapshot.
        let connection = connection.unchecked_transaction().map_err(database_error)?;
        let mut statement = connection.prepare(&statement_sql).map_err(database_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                let provider = row
                    .get::<_, String>(0)?
                    .parse::<Provider>()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
                    })?;
                let indexed_byte_count = row.get::<_, i64>(4)?;
                let indexed_byte_count = usize::try_from(indexed_byte_count).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(4, Type::Integer, Box::new(error))
                })?;
                let trajectory_match = SessionTrajectoryMatch {
                    session: SessionRef::new(provider, row.get::<_, String>(1)?),
                    snippet: String::new(),
                    source_complete: row.get(2)?,
                    complete: row.get(3)?,
                    indexed_byte_count,
                    source_byte_count: usize::try_from(row.get::<_, i64>(5)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, Box::new(error))
                    })?,
                    truncation_strategy: row.get(6)?,
                };
                Ok((
                    trajectory_match,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(database_error)?;
        let mut rows = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let matches = with_trajectory_excerpts(&connection, &clauses, rows)?;
        Ok(SessionTrajectorySearchPage { matches, has_more })
    }

    /// Creates or upgrades the schema required by this store.
    ///
    /// # Errors
    ///
    /// Returns an error when schema creation or upgrade fails.
    pub fn initialize_schema(&self) -> Result<()> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let stored_format = transaction
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(database_error)?;
        if stored_format > STORE_FORMAT {
            return Err(StoreError::NewerStoreFormat);
        }
        transaction
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS tasks (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
                    workspace_root TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    UNIQUE (workspace_root, name)
                );

                CREATE TABLE IF NOT EXISTS task_selections (
                    workspace_root TEXT PRIMARY KEY,
                    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                    selected_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_bindings (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                    branch_name TEXT NOT NULL CHECK (length(trim(branch_name)) > 0),
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                    bound_at INTEGER NOT NULL,
                    is_current INTEGER NOT NULL DEFAULT 1 CHECK (is_current IN (0, 1))
                );

                CREATE UNIQUE INDEX IF NOT EXISTS current_session_binding_per_branch
                    ON session_bindings (task_id, branch_name)
                    WHERE is_current = 1;

                CREATE INDEX IF NOT EXISTS session_binding_lineage
                    ON session_bindings (task_id, branch_name, id);

                CREATE TABLE IF NOT EXISTS handoffs (
                    id INTEGER PRIMARY KEY,
                    source_provider TEXT NOT NULL,
                    source_session_id TEXT NOT NULL CHECK (length(source_session_id) > 0),
                    target_provider TEXT NOT NULL,
                    target_session_id TEXT NOT NULL CHECK (length(target_session_id) > 0),
                    mode TEXT NOT NULL,
                    fidelity_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS bundles (
                    bundle_id TEXT PRIMARY KEY,
                    bundle_json TEXT NOT NULL,
                    saved_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_index (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                    title TEXT,
                    project_path TEXT,
                    git_branch TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    updated_at_approximate INTEGER NOT NULL DEFAULT 0
                        CHECK (updated_at_approximate IN (0, 1)),
                    event_count INTEGER NOT NULL CHECK (event_count >= 0),
                    indexed_at INTEGER NOT NULL,
                    PRIMARY KEY (provider, session_id)
                ) WITHOUT ROWID;

                CREATE INDEX IF NOT EXISTS session_index_updated
                    ON session_index (updated_at DESC);
                CREATE INDEX IF NOT EXISTS session_index_provider_updated
                    ON session_index (provider, updated_at DESC);

                CREATE TABLE IF NOT EXISTS session_index_state (
                    provider TEXT PRIMARY KEY,
                    indexed_at INTEGER NOT NULL
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS session_index_checks (
                    provider TEXT PRIMARY KEY,
                    checked_at INTEGER NOT NULL
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS trajectory_index_failures (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                    source_updated_at INTEGER,
                    document_version INTEGER NOT NULL CHECK (document_version >= 0),
                    failed_at INTEGER NOT NULL,
                    PRIMARY KEY (provider, session_id)
                ) WITHOUT ROWID;

                ",
            )
            .map_err(database_error)?;
        ensure_session_index_approximation_column(&transaction)?;
        initialize_bundle_source_index(&transaction)?;
        initialize_trajectory_schema(&transaction)?;
        ensure_source_time_columns(&transaction)?;
        initialize_source_revision_schema(&transaction)?;
        if stored_format < STORE_FORMAT {
            transaction
                .pragma_update(None, "user_version", STORE_FORMAT)
                .map_err(database_error)?;
        }
        transaction.commit().map_err(database_error)
    }

    fn open_state_root(state_root: &Path) -> Result<Self> {
        if state_root.as_os_str().is_empty() {
            return Err(StoreError::DefaultDirectoryUnavailable);
        }
        fs::create_dir_all(state_root).map_err(|_| filesystem_error())?;
        if !fs::metadata(state_root)
            .map_err(|_| filesystem_error())?
            .is_dir()
        {
            return Err(filesystem_error());
        }
        reject_symlink(state_root)?;
        set_private_directory(state_root)?;
        Self::open(state_root.join(DATABASE_FILE_NAME))
    }
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(filesystem_error()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(filesystem_error()),
    }
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| filesystem_error())
}

#[cfg(not(unix))]
fn set_private_directory(path: &Path) -> Result<()> {
    fs::metadata(path)
        .map_err(|_| filesystem_error())?
        .is_dir()
        .then_some(())
        .ok_or_else(filesystem_error)
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| filesystem_error())
}

#[cfg(not(unix))]
fn set_private_file(path: &Path) -> Result<()> {
    fs::metadata(path)
        .map_err(|_| filesystem_error())?
        .is_file()
        .then_some(())
        .ok_or_else(filesystem_error)
}

/// Longest time one store statement waits for another connection's lock.
const BUSY_WAIT_LIMIT: Duration = Duration::from_secs(30);
/// Shortest pause between attempts to acquire a locked database.
const BUSY_RETRY_MIN_PAUSE_MICROS: u64 = 500;
/// Random extra pause that keeps concurrent waiters from retrying in lockstep.
const BUSY_RETRY_JITTER_MICROS: u64 = 4_000;

thread_local! {
    static BUSY_WAIT_STARTED: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Retries a locked database with short jittered pauses until [`BUSY_WAIT_LIMIT`] elapses.
///
/// `SQLite`'s default busy handler sleeps up to 100 ms between attempts, and Windows rounds those
/// sleeps up to scheduler ticks. Under contention a waiting writer then misses the short gaps in
/// which other connections release the lock and fails with `SQLITE_BUSY` once its timeout expires.
/// Polling every few milliseconds keeps waiters competitive; the deadline is measured in wall-clock
/// time from the first attempt of each busy episode, which always runs on the calling thread.
fn retry_busy_database(attempt: i32) -> bool {
    let now = Instant::now();
    let started = BUSY_WAIT_STARTED.with(|cell| {
        let started = match cell.get() {
            Some(started) if attempt > 0 => started,
            _ => now,
        };
        cell.set(Some(started));
        started
    });
    if now.duration_since(started) >= BUSY_WAIT_LIMIT {
        return false;
    }
    let mut jitter = RandomState::new().build_hasher();
    jitter.write_i32(attempt);
    std::thread::sleep(Duration::from_micros(
        BUSY_RETRY_MIN_PAUSE_MICROS + jitter.finish() % BUSY_RETRY_JITTER_MICROS,
    ));
    true
}

fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)
}

fn empty_trajectory_search_page() -> SessionTrajectorySearchPage {
    SessionTrajectorySearchPage {
        matches: Vec::new(),
        has_more: false,
    }
}

/// Builds one ranked page statement.
///
/// Parameters are `?1..?N` FTS expressions, `?N+1` eligibility JSON, `?N+2` limit, then one folded
/// phrase per exact clause, in clause order. Rows end with the snippet's chunk ID and clause index.
/// Exact clauses filter candidate chunks with [`FOLDED_CONTAINS_FUNCTION`] and leave the snippet
/// empty for the caller to cut around the phrase.
fn trajectory_search_page_sql(clauses: &[TrajectoryClause]) -> String {
    let eligibility_parameter = clauses.len() + 1;
    let limit_parameter = clauses.len() + 2;
    let mut exact_parameter = limit_parameter;
    let exact_filters = clauses
        .iter()
        .map(|clause| {
            if clause.exact_phrase.is_none() {
                return String::new();
            }
            exact_parameter += 1;
            format!(" AND {FOLDED_CONTAINS_FUNCTION}(chunks.stored_text, ?{exact_parameter})")
        })
        .collect::<Vec<_>>();
    let eligible = format!(
        "WITH eligible(provider, session_id, file, contents) AS (
             SELECT provider, session_id, NULL, NULL FROM session_trajectories
             WHERE ?{eligibility_parameter} IS NULL
             UNION ALL
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]'),
                    json_extract(value, '$[2]'), json_extract(value, '$[3]')
             FROM json_each(?{eligibility_parameter})
             WHERE ?{eligibility_parameter} IS NOT NULL
         ),"
    );
    if let [clause] = clauses {
        single_clause_page_sql(eligible, clause, &exact_filters[0], limit_parameter)
    } else {
        multi_clause_page_sql(eligible, clauses, &exact_filters, limit_parameter)
    }
}

// Excerpts tokenize whole chunks, so they are built for delivered rows only. FTS5 snippet() can
// center on a token-equal but different occurrence, so exact snippets are cut around the phrase
// itself.
fn with_trajectory_excerpts(
    connection: &Connection,
    clauses: &[TrajectoryClause],
    rows: Vec<(SessionTrajectoryMatch, i64, i64)>,
) -> Result<Vec<SessionTrajectoryMatch>> {
    let mut delivered = Vec::with_capacity(rows.len());
    for (trajectory_match, chunk_id, clause_index) in rows {
        let (clause_index, clause) = usize::try_from(clause_index)
            .ok()
            .and_then(|index| clauses.get(index).map(|clause| (index, clause)))
            .ok_or(StoreError::CorruptStore)?;
        delivered.push((trajectory_match, chunk_id, clause_index, clause));
    }
    // One full-text scan per clause, because a MATCH per row re-seeks every index segment.
    let mut fts_snippet = connection
        .prepare(&format!(
            "SELECT rowid, {FTS_SNIPPET} FROM session_trajectory_chunks_fts
             WHERE session_trajectory_chunks_fts MATCH ?1
               AND +rowid IN (SELECT value FROM json_each(?2))"
        ))
        .map_err(database_error)?;
    let mut fts_snippets = std::collections::HashMap::new();
    for (clause_index, clause) in clauses.iter().enumerate() {
        let chunk_ids = delivered
            .iter()
            .filter(|(_, _, index, _)| *index == clause_index)
            .map(|(_, chunk_id, _, _)| *chunk_id)
            .collect::<Vec<_>>();
        if clause.exact_phrase.is_some() || chunk_ids.is_empty() {
            continue;
        }
        let chunk_ids = serde_json::to_string(&chunk_ids)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        let mut snippets = fts_snippet
            .query(rusqlite::params![clause.match_expression, chunk_ids])
            .map_err(database_error)?;
        while let Some(row) = snippets.next().map_err(database_error)? {
            let chunk_id = row.get::<_, i64>(0).map_err(database_error)?;
            let snippet = row.get::<_, String>(1).map_err(database_error)?;
            fts_snippets.insert((clause_index, chunk_id), snippet);
        }
    }
    let mut stored_chunk = clauses
        .iter()
        .any(|clause| clause.exact_phrase.is_some())
        .then(|| {
            connection
                .prepare("SELECT stored_text FROM session_trajectory_chunk_rows WHERE id = ?1")
        })
        .transpose()
        .map_err(database_error)?;
    let mut scratch = Vec::new();
    let mut matches = Vec::with_capacity(delivered.len());
    for (mut trajectory_match, chunk_id, clause_index, clause) in delivered {
        if let (Some(phrase), Some(statement)) =
            (clause.exact_phrase.as_deref(), stored_chunk.as_mut())
        {
            trajectory_match.snippet = statement
                .query_row([chunk_id], |row| {
                    let text = chunk_text(row.get_ref(0)?, &mut scratch).unwrap_or_default();
                    Ok(exact_phrase_snippet(text, phrase))
                })
                .optional()
                .map_err(database_error)?
                .unwrap_or_default();
        } else if let Some(snippet) = fts_snippets.remove(&(clause_index, chunk_id)) {
            trajectory_match.snippet = snippet;
        }
        matches.push(trajectory_match);
    }
    Ok(matches)
}

fn single_clause_page_sql(
    mut sql: String,
    clause: &TrajectoryClause,
    exact_filter: &str,
    limit_parameter: usize,
) -> String {
    let candidate_chunks = trajectory_candidate_chunks_sql(clause);
    write!(
        sql,
        "ranked_chunks AS (
             SELECT chunks.id AS chunk_id, chunks.trajectory_id,
                    session_trajectory_chunks_fts.rank AS match_rank,
                    row_number() OVER (
                        PARTITION BY chunks.trajectory_id
                        ORDER BY session_trajectory_chunks_fts.rank, chunks.chunk_index
                    ) AS chunk_rank
             {candidate_chunks}
             WHERE session_trajectory_chunks_fts MATCH ?1{exact_filter}
         )
         SELECT trajectories.provider, trajectories.session_id,
                trajectories.source_complete, trajectories.complete,
                trajectories.indexed_byte_count, trajectories.source_byte_count,
                trajectories.truncation_strategy, ranked_chunks.chunk_id, 0
         FROM ranked_chunks
         INNER JOIN session_trajectories AS trajectories
             ON trajectories.id = ranked_chunks.trajectory_id
         WHERE chunk_rank = 1
         ORDER BY match_rank, trajectories.source_updated_at DESC,
                  trajectories.provider, trajectories.session_id
         LIMIT ?{limit_parameter}"
    )
    .expect("writing SQL into a string cannot fail");
    sql
}

fn multi_clause_page_sql(
    mut sql: String,
    clauses: &[TrajectoryClause],
    exact_filters: &[String],
    limit_parameter: usize,
) -> String {
    let clause_count = clauses.len();
    sql.push_str(
        "clause_matches(trajectory_id, chunk_id, clause_index, clause_rank, exact_clause) AS (",
    );
    for (clause_index, (clause, exact_filter)) in clauses.iter().zip(exact_filters).enumerate() {
        if clause_index != 0 {
            sql.push_str(" UNION ALL ");
        }
        let parameter = clause_index + 1;
        let exact_clause = i32::from(clause.exact_phrase.is_some());
        let candidate_chunks = trajectory_candidate_chunks_sql(clause);
        write!(
            sql,
            "SELECT chunks.trajectory_id, chunks.id, {clause_index},
                    session_trajectory_chunks_fts.rank, {exact_clause}
             {candidate_chunks}
             WHERE session_trajectory_chunks_fts MATCH ?{parameter}{exact_filter}"
        )
        .expect("writing SQL into a string cannot fail");
    }
    write!(
        sql,
        "),
         best_clause_matches AS (
             SELECT clause_matches.*,
                    row_number() OVER (
                        PARTITION BY trajectory_id, clause_index
                        ORDER BY clause_rank, chunk_id
                    ) AS clause_rank_order
             FROM clause_matches
         ),
         qualified_trajectories AS (
             SELECT trajectory_id, sum(clause_rank) AS trajectory_rank
             FROM best_clause_matches
             WHERE clause_rank_order = 1
             GROUP BY trajectory_id
             HAVING count(*) = {clause_count}
         ),
         ranked_trajectories AS (
             SELECT qualified_trajectories.trajectory_id,
                    qualified_trajectories.trajectory_rank
             FROM qualified_trajectories
             INNER JOIN session_trajectories AS trajectories
                 ON trajectories.id = qualified_trajectories.trajectory_id
             ORDER BY qualified_trajectories.trajectory_rank,
                      trajectories.source_updated_at DESC,
                      trajectories.provider, trajectories.session_id
             LIMIT ?{limit_parameter}
         ),
         best_snippets AS (
             SELECT best_clause_matches.*,
                    row_number() OVER (
                        PARTITION BY best_clause_matches.trajectory_id
                        ORDER BY exact_clause DESC, clause_rank, clause_index, chunk_id
                    ) AS snippet_rank
             FROM best_clause_matches
             INNER JOIN ranked_trajectories
                 ON ranked_trajectories.trajectory_id =
                    best_clause_matches.trajectory_id
             WHERE clause_rank_order = 1
         )
         SELECT trajectories.provider, trajectories.session_id,
                trajectories.source_complete, trajectories.complete,
                trajectories.indexed_byte_count, trajectories.source_byte_count,
                trajectories.truncation_strategy,
                best_snippets.chunk_id, best_snippets.clause_index
         FROM ranked_trajectories
         INNER JOIN best_snippets
             ON best_snippets.trajectory_id = ranked_trajectories.trajectory_id
            AND best_snippets.snippet_rank = 1
         INNER JOIN session_trajectories AS trajectories
             ON trajectories.id = ranked_trajectories.trajectory_id
         ORDER BY ranked_trajectories.trajectory_rank,
                  trajectories.source_updated_at DESC,
                  trajectories.provider, trajectories.session_id"
    )
    .expect("writing SQL into a string cannot fail");
    sql
}

fn validate_catalog_rows(provider: Provider, sessions: &[IndexedSession]) -> Result<()> {
    if sessions
        .iter()
        .any(|entry| entry.session.provider != provider || entry.session.id.is_empty())
    {
        return Err(StoreError::InvalidSessionReference);
    }
    if sessions.iter().any(|entry| {
        entry
            .project_path
            .as_deref()
            .is_some_and(|path| path.to_str().is_none())
    }) {
        return Err(StoreError::InvalidWorkspaceRoot);
    }
    Ok(())
}

fn catalog_rows_by_id(
    connection: &Connection,
    provider: Provider,
) -> Result<HashMap<String, CatalogRowState>> {
    let mut rows = HashMap::<String, CatalogRowState>::new();
    for row in Store::read_indexed_sessions(connection, Some(provider), false)? {
        let id = row.session.id.clone();
        rows.entry(id).or_default().metadata = Some(row);
    }
    let mut statement = connection
        .prepare_cached(
            "SELECT session_id, source_updated_at, document_version, source_updated_submillis,
                    source_file_id, source_contents_id FROM trajectory_index_failures
             WHERE provider = ?1 AND source_file_id IS NOT NULL AND source_contents_id IS NOT NULL",
        )
        .map_err(database_error)?;
    let failures = statement
        .query_map([provider.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, failure_from_row(row, 1)?))
        })
        .map_err(database_error)?;
    for failure in failures {
        let (id, failure) = failure.map_err(database_error)?;
        rows.entry(id).or_default().failure = Some(failure);
    }
    Ok(rows)
}

fn catalog_id_counts(sessions: &[IndexedSession]) -> HashMap<&str, usize> {
    let mut counts = HashMap::with_capacity(sessions.len());
    for row in sessions {
        *counts.entry(row.session.id.as_str()).or_default() += 1;
    }
    counts
}

fn write_discovered_sessions(
    transaction: &Transaction<'_>,
    provider: Provider,
    sessions: &[IndexedSession],
    complete: bool,
    indexed_at: i64,
) -> Result<()> {
    let provider_name = provider.to_string();
    if complete && TrajectorySourceGuard::uses_file_time(provider) {
        // Keep listed entries in place so old workspaces remain paired with their search text.
        let ids = sessions
            .iter()
            .map(|entry| &entry.session.id)
            .collect::<Vec<_>>();
        let ids = serde_json::to_string(&ids)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        transaction
            .execute(
                "DELETE FROM session_index WHERE provider = ?1
                 AND session_id NOT IN (SELECT value FROM json_each(?2))",
                params![provider_name, ids],
            )
            .map_err(database_error)?;
    } else if complete {
        transaction
            .execute(
                "DELETE FROM session_index WHERE provider = ?1",
                [&provider_name],
            )
            .map_err(database_error)?;
    }
    {
        let mut upsert = prepare_indexed_session(transaction)?;
        for session in sessions {
            write_indexed_session_row(&mut upsert, session, indexed_at, false)?;
        }
    }
    if complete {
        transaction
            .execute(
                "
                    INSERT INTO session_index_state (provider, indexed_at)
                    VALUES (?1, ?2)
                    ON CONFLICT (provider) DO UPDATE SET indexed_at = excluded.indexed_at
                    ",
                params![provider_name, indexed_at],
            )
            .map_err(database_error)?;
    }
    Ok(())
}

fn catalog_generation(connection: &Connection, provider: Provider) -> Result<i64> {
    let mut query = connection
        .prepare_cached(
            "SELECT coalesce((SELECT generation FROM session_catalog_generations WHERE provider = ?1), 0)",
        )
        .map_err(database_error)?;
    query
        .query_row([provider.to_string()], |row| row.get(0))
        .map_err(database_error)
}

fn provider_index_matches(
    connection: &Connection,
    provider: Provider,
    sessions: &[IndexedSession],
) -> Result<bool> {
    let refreshed: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_index_state WHERE provider = ?1)",
            [provider.to_string()],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if !refreshed {
        return Ok(false);
    }
    // Compare all stored rows so complete discovery can prune superseded catalog entries.
    let mut existing = Store::read_indexed_sessions(connection, Some(provider), false)?;
    let mut incoming = sessions.iter().collect::<Vec<_>>();
    existing.sort_by(|left, right| left.session.id.cmp(&right.session.id));
    incoming.sort_by(|left, right| left.session.id.cmp(&right.session.id));
    Ok(existing.len() == incoming.len()
        && existing
            .iter()
            .zip(incoming)
            .all(|(left, right)| left == right))
}

fn prune_stale_native_trajectories(transaction: &Transaction<'_>, provider: &str) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM session_trajectories
             WHERE provider = ?1
               AND protected_by_bundle = 0
               AND NOT EXISTS (
                   SELECT 1 FROM session_index
                   WHERE session_index.provider = session_trajectories.provider
                     AND session_index.session_id = session_trajectories.session_id
               )",
            params![provider],
        )
        .map_err(database_error)?;
    Ok(())
}

fn ensure_session_index_approximation_column(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction
        .prepare("PRAGMA table_info(session_index)")
        .map_err(database_error)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(database_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(database_error)?;
    drop(statement);
    if columns
        .iter()
        .any(|column| column == "updated_at_approximate")
    {
        return Ok(());
    }
    transaction
        .execute_batch(
            "ALTER TABLE session_index
             ADD COLUMN updated_at_approximate INTEGER NOT NULL DEFAULT 0
             CHECK (updated_at_approximate IN (0, 1));",
        )
        .map_err(database_error)
}

fn initialize_catalog_generations(transaction: &Transaction<'_>) -> Result<()> {
    // Older clients can write these rows too. A refresh that changes only indexed_at is harmless.
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_catalog_generations(
            provider TEXT PRIMARY KEY, generation INTEGER NOT NULL);
         CREATE TRIGGER IF NOT EXISTS tracked_catalog_insert AFTER INSERT ON session_index
         WHEN NEW.provider IN ('pi','omp')
         BEGIN
           INSERT INTO session_catalog_generations VALUES (NEW.provider, 1)
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;
         CREATE TRIGGER IF NOT EXISTS tracked_catalog_delete AFTER DELETE ON session_index
         WHEN OLD.provider IN ('pi','omp')
         BEGIN
           INSERT INTO session_catalog_generations VALUES (OLD.provider, 1)
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;
         CREATE TRIGGER IF NOT EXISTS tracked_catalog_update AFTER UPDATE ON session_index
         WHEN (OLD.provider IN ('pi','omp') OR NEW.provider IN ('pi','omp')) AND
            (OLD.provider, OLD.session_id, OLD.title, OLD.project_path, OLD.git_branch, OLD.created_at,
             OLD.updated_at, OLD.updated_at_approximate, OLD.event_count, OLD.source_updated_submillis,
             OLD.source_file_id, OLD.source_contents_id) IS NOT
            (NEW.provider, NEW.session_id, NEW.title, NEW.project_path, NEW.git_branch, NEW.created_at,
             NEW.updated_at, NEW.updated_at_approximate, NEW.event_count, NEW.source_updated_submillis,
             NEW.source_file_id, NEW.source_contents_id)
         BEGIN
           INSERT INTO session_catalog_generations(provider, generation)
           SELECT provider, 1 FROM (SELECT OLD.provider AS provider UNION SELECT NEW.provider)
           WHERE provider IN ('pi','omp')
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;
         CREATE TRIGGER IF NOT EXISTS tracked_failure_insert AFTER INSERT ON trajectory_index_failures
         WHEN NEW.provider IN ('pi','omp') AND NEW.source_file_id IS NOT NULL AND NEW.source_contents_id IS NOT NULL
         BEGIN
           INSERT INTO session_catalog_generations VALUES (NEW.provider, 1)
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;
         CREATE TRIGGER IF NOT EXISTS tracked_failure_delete AFTER DELETE ON trajectory_index_failures
         WHEN OLD.provider IN ('pi','omp') AND OLD.source_file_id IS NOT NULL AND OLD.source_contents_id IS NOT NULL
         BEGIN
           INSERT INTO session_catalog_generations VALUES (OLD.provider, 1)
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;
         CREATE TRIGGER IF NOT EXISTS tracked_failure_update AFTER UPDATE ON trajectory_index_failures
         WHEN ((OLD.provider IN ('pi','omp') AND OLD.source_file_id IS NOT NULL AND OLD.source_contents_id IS NOT NULL)
            OR (NEW.provider IN ('pi','omp') AND NEW.source_file_id IS NOT NULL AND NEW.source_contents_id IS NOT NULL))
           AND (OLD.provider, OLD.session_id, OLD.source_updated_at, OLD.source_updated_submillis,
                OLD.document_version, OLD.source_file_id, OLD.source_contents_id) IS NOT
               (NEW.provider, NEW.session_id, NEW.source_updated_at, NEW.source_updated_submillis,
                NEW.document_version, NEW.source_file_id, NEW.source_contents_id)
         BEGIN
           INSERT INTO session_catalog_generations(provider, generation)
           SELECT provider, 1 FROM (
             SELECT OLD.provider AS provider WHERE OLD.source_file_id IS NOT NULL AND OLD.source_contents_id IS NOT NULL
             UNION SELECT NEW.provider WHERE NEW.source_file_id IS NOT NULL AND NEW.source_contents_id IS NOT NULL)
           WHERE provider IN ('pi','omp')
           ON CONFLICT (provider) DO UPDATE SET generation = generation + 1;
         END;"
    ).map_err(database_error)
}

fn initialize_source_revision_schema(transaction: &Transaction<'_>) -> Result<()> {
    for (table, column) in [
        ("session_index", "source_file_id"),
        ("session_index", "source_contents_id"),
        ("session_trajectories", "source_file_id"),
        ("session_trajectories", "missing_ancestor_id"),
        ("session_trajectories", "source_contents_id"),
        ("trajectory_index_failures", "source_file_id"),
        ("trajectory_index_failures", "source_contents_id"),
    ] {
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                params![table, column],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        if !exists {
            transaction
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} BLOB"))
                .map_err(database_error)?;
        }
    }
    initialize_catalog_generations(transaction)
}

fn source_revision_from_row(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<SourceRevision> {
    Ok(SourceRevision {
        file: optional_uuid(row, column)?,
        missing_ancestor: optional_uuid(row, column + 1)?,
        contents: optional_uuid(row, column + 2)?,
    })
}

fn ensure_source_time_columns(transaction: &Transaction<'_>) -> Result<()> {
    // Existing rows keep a NULL remainder until their file is next read.
    for table in [
        "session_trajectories",
        "trajectory_index_failures",
        "session_index",
    ] {
        let present = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = 'source_updated_submillis')",
            [table],
            |row| row.get::<_, bool>(0),
        ).map_err(database_error)?;
        if !present {
            transaction
                .execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN source_updated_submillis INTEGER
                 CHECK (source_updated_submillis BETWEEN 0 AND 999999)"
                ))
                .map_err(database_error)?;
            if table == "session_index" {
                transaction
                    .execute_batch(
                        "DELETE FROM session_index_state WHERE provider = 'omp';
                    DELETE FROM session_index_checks WHERE provider = 'omp';",
                    )
                    .map_err(database_error)?;
            }
        }
    }
    Ok(())
}

fn initialize_trajectory_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_trajectories (
                id INTEGER PRIMARY KEY,
                provider TEXT NOT NULL,
                session_id TEXT NOT NULL CHECK (length(session_id) > 0),
                redacted_text TEXT NOT NULL,
                content_hash BLOB NOT NULL,
                source_updated_at INTEGER NOT NULL,
                source_complete INTEGER NOT NULL CHECK (source_complete IN (0, 1)),
                complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
                indexed_at INTEGER NOT NULL,
                source_byte_count INTEGER NOT NULL CHECK (source_byte_count >= 0),
                indexed_byte_count INTEGER NOT NULL CHECK (indexed_byte_count >= 0),
                truncation_strategy TEXT NOT NULL,
                origin TEXT NOT NULL CHECK (origin IN ('native', 'imported_bundle')),
                protected_by_bundle INTEGER NOT NULL
                    CHECK (protected_by_bundle IN (0, 1)),
                UNIQUE (provider, session_id)
            );

            ",
        )
        .map_err(database_error)?;
    ensure_trajectory_coverage_columns(transaction)?;
    // Older writers replace the content without recording which reader produced it.
    // A current writer restores the marker after the upsert in the same transaction.
    transaction
        .execute_batch(
            "CREATE TRIGGER IF NOT EXISTS reset_tail_reader_after_write
             AFTER UPDATE OF content_hash ON session_trajectories WHEN OLD.tail_reader_version <> 0
             BEGIN
                 UPDATE session_trajectories SET tail_reader_version = 0 WHERE id = NEW.id;
             END;",
        )
        .map_err(database_error)?;
    initialize_trajectory_chunk_schema(transaction)
}

fn ensure_trajectory_coverage_columns(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction
        .prepare("PRAGMA table_info(session_trajectories)")
        .map_err(database_error)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(database_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(database_error)?;
    drop(statement);
    let has_column = |name: &str| columns.iter().any(|column| column == name);
    let missing_coverage = !has_column("source_byte_count")
        || !has_column("indexed_byte_count")
        || !has_column("truncation_strategy")
        || !has_column("source_complete");
    if !has_column("tail_reader_version") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN tail_reader_version INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(database_error)?;
    }
    if !has_column("content_hash") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN content_hash BLOB NOT NULL DEFAULT X'';",
            )
            .map_err(database_error)?;
    }
    if !has_column("source_complete") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN source_complete INTEGER NOT NULL DEFAULT 0
                 CHECK (source_complete IN (0, 1));",
            )
            .map_err(database_error)?;
    }
    if !has_column("source_byte_count") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN source_byte_count INTEGER NOT NULL DEFAULT 0
                 CHECK (source_byte_count >= 0);",
            )
            .map_err(database_error)?;
    }
    if !has_column("indexed_byte_count") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN indexed_byte_count INTEGER NOT NULL DEFAULT 0
                 CHECK (indexed_byte_count >= 0);",
            )
            .map_err(database_error)?;
    }
    if !has_column("truncation_strategy") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN truncation_strategy TEXT NOT NULL DEFAULT 'legacy_unknown';",
            )
            .map_err(database_error)?;
    }
    if !has_column("origin") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN origin TEXT NOT NULL DEFAULT 'native'
                 CHECK (origin IN ('native', 'imported_bundle'));",
            )
            .map_err(database_error)?;
    }
    if !has_column("protected_by_bundle") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN protected_by_bundle INTEGER NOT NULL DEFAULT 0
                 CHECK (protected_by_bundle IN (0, 1));",
            )
            .map_err(database_error)?;
    }
    if !has_column("provider_limited") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN provider_limited INTEGER NOT NULL DEFAULT 0 CHECK (provider_limited IN (0, 1))").map_err(database_error)?;
    }
    ensure_trajectory_version_columns(transaction, &columns)?;
    if missing_coverage {
        transaction
            .execute_batch(
                "UPDATE session_trajectories SET
                    source_byte_count = length(CAST(redacted_text AS BLOB)),
                    indexed_byte_count = length(CAST(redacted_text AS BLOB)),
                    truncation_strategy = 'legacy_unknown',
                    source_complete = 0,
                    complete = 0;",
            )
            .map_err(database_error)?;
    }
    protect_bundle_trajectories(transaction)
}

fn ensure_trajectory_version_columns(
    transaction: &Transaction<'_>,
    columns: &[String],
) -> Result<()> {
    if !columns.iter().any(|column| column == "document_version") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN document_version INTEGER NOT NULL DEFAULT 0
                 CHECK (document_version >= 0);",
            )
            .map_err(database_error)?;
    }
    if !columns.iter().any(|column| column == "derived_title") {
        transaction
            .execute_batch("ALTER TABLE session_trajectories ADD COLUMN derived_title TEXT;")
            .map_err(database_error)?;
    }
    if !columns.iter().any(|column| column == "title_authoritative") {
        transaction
            .execute_batch("ALTER TABLE session_trajectories ADD COLUMN title_authoritative INTEGER CHECK (title_authoritative IN (0, 1));")
            .map_err(database_error)?;
    }
    Ok(())
}

/// Provider of the session a stored bundle was made from, read from the bundle JSON.
const BUNDLE_SOURCE_PROVIDER_SQL: &str = "json_extract(
    CASE WHEN json_valid(bundle_json) THEN bundle_json END, '$.snapshot.session.provider')";
/// ID of the session a stored bundle was made from, read from the bundle JSON.
const BUNDLE_SOURCE_SESSION_ID_SQL: &str = "json_extract(
    CASE WHEN json_valid(bundle_json) THEN bundle_json END, '$.snapshot.session.id')";

/// Indexes the source session of each bundle.
///
/// `protect_bundle_trajectories` runs every time a store opens. Without this index it parsed the
/// JSON of every bundle once for each trajectory, so one large bundle made every command slow.
/// The index holds both values, and `SQLite` fills it when any version of this program saves a
/// bundle.
fn initialize_bundle_source_index(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS bundle_source_session ON bundles (
                 {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
             );"
        ))
        .map_err(database_error)
}

// The expressions in the subquery must stay identical to the indexed ones, so that `SQLite` reads
// them from `bundle_source_session` instead of parsing bundle JSON. Rows that are already
// protected are skipped, so an unchanged store is not written.
fn protect_bundle_trajectories(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(&format!(
            "UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE protected_by_bundle = 0 AND (
                 origin = 'imported_bundle' OR (provider, session_id) IN (
                     SELECT {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
                     FROM bundles
                 )
             );"
        ))
        .map_err(database_error)
}

fn protect_bundle_trajectory(transaction: &Transaction<'_>, session: &SessionRef) -> Result<()> {
    transaction
        .execute(
            "UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE provider = ?1 AND session_id = ?2",
            params![session.provider.to_string(), session.id],
        )
        .map_err(database_error)?;
    Ok(())
}

/// Moves a chunk table that holds only plain text to the layout that can hold compressed text.
///
/// The FTS5 index reads chunk text from `session_trajectory_chunks` by name. The table takes a
/// new name, and a view with the old name returns the text of each row, so the index stays valid
/// and is not rebuilt. Rows keep their plain text until their session is indexed again. A version
/// without compression cannot open the store after this, because it does not have the SQL
/// function that the view calls.
fn rename_plain_chunk_table(transaction: &Transaction<'_>) -> Result<()> {
    let plain_table = transaction
        .query_row(
            "SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'session_trajectory_chunks'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(database_error)?
        .is_some();
    if !plain_table {
        return Ok(());
    }
    transaction
        .execute_batch(
            "DROP TRIGGER IF EXISTS session_trajectory_chunks_after_insert;
             DROP TRIGGER IF EXISTS session_trajectory_chunks_after_delete;
             DROP TRIGGER IF EXISTS session_trajectory_chunks_after_update;
             ALTER TABLE session_trajectory_chunks RENAME TO session_trajectory_chunk_rows;
             ALTER TABLE session_trajectory_chunk_rows
                 RENAME COLUMN redacted_text TO stored_text;",
        )
        .map_err(database_error)
}

fn initialize_trajectory_chunk_schema(transaction: &Transaction<'_>) -> Result<()> {
    rename_plain_chunk_table(transaction)?;
    transaction
        .execute_batch(
            "DROP TRIGGER IF EXISTS session_trajectories_after_insert;
             DROP TRIGGER IF EXISTS session_trajectories_after_delete;
             DROP TRIGGER IF EXISTS session_trajectories_after_update;
             DROP TABLE IF EXISTS session_trajectories_fts;

             CREATE TABLE IF NOT EXISTS session_trajectory_chunk_rows (
                 id INTEGER PRIMARY KEY,
                 trajectory_id INTEGER NOT NULL
                     REFERENCES session_trajectories(id) ON DELETE CASCADE,
                 chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
                 stored_text TEXT NOT NULL,
                 UNIQUE (trajectory_id, chunk_index)
             );

             CREATE INDEX IF NOT EXISTS session_trajectory_chunk_owner
                 ON session_trajectory_chunk_rows (id, trajectory_id, chunk_index);

             CREATE VIEW IF NOT EXISTS session_trajectory_chunks AS
                 SELECT id, trajectory_id, chunk_index,
                        omnis_chunk_text(stored_text) AS redacted_text
                 FROM session_trajectory_chunk_rows;

             CREATE VIRTUAL TABLE IF NOT EXISTS session_trajectory_chunks_fts USING fts5(
                 redacted_text,
                 content = 'session_trajectory_chunks',
                 content_rowid = 'id',
                 tokenize = 'unicode61 remove_diacritics 2'
             );

             CREATE TRIGGER IF NOT EXISTS session_trajectory_chunk_rows_after_insert
             AFTER INSERT ON session_trajectory_chunk_rows BEGIN
                 INSERT INTO session_trajectory_chunks_fts (rowid, redacted_text)
                 VALUES (new.id, omnis_chunk_text(new.stored_text));
             END;

             CREATE TRIGGER IF NOT EXISTS session_trajectory_chunk_rows_after_delete
             AFTER DELETE ON session_trajectory_chunk_rows BEGIN
                 INSERT INTO session_trajectory_chunks_fts (
                     session_trajectory_chunks_fts, rowid, redacted_text
                 ) VALUES ('delete', old.id, omnis_chunk_text(old.stored_text));
             END;

             CREATE TABLE IF NOT EXISTS session_trajectory_chunk_repairs (
                 id INTEGER PRIMARY KEY CHECK (id = 1)
             );

             CREATE TRIGGER IF NOT EXISTS session_trajectory_chunk_rows_unreadable_delete
             AFTER DELETE ON session_trajectory_chunk_rows
             WHEN NOT omnis_chunk_readable(old.stored_text) BEGIN
                 INSERT OR IGNORE INTO session_trajectory_chunk_repairs (id) VALUES (1);
             END;

             CREATE TRIGGER IF NOT EXISTS session_trajectory_chunk_rows_unreadable_update
             AFTER UPDATE ON session_trajectory_chunk_rows
             WHEN NOT omnis_chunk_readable(old.stored_text) BEGIN
                 INSERT OR IGNORE INTO session_trajectory_chunk_repairs (id) VALUES (1);
             END;

             CREATE TRIGGER IF NOT EXISTS session_trajectory_chunk_rows_after_update
             AFTER UPDATE ON session_trajectory_chunk_rows BEGIN
                 INSERT INTO session_trajectory_chunks_fts (
                     session_trajectory_chunks_fts, rowid, redacted_text
                 ) VALUES ('delete', old.id, omnis_chunk_text(old.stored_text));
                 INSERT INTO session_trajectory_chunks_fts (rowid, redacted_text)
                 VALUES (new.id, omnis_chunk_text(new.stored_text));
             END;",
        )
        .map_err(database_error)?;
    // The setting is stored in the index. It is written only when the stored value differs, so
    // an open does not write to an unchanged store.
    transaction
        .execute(
            "INSERT INTO session_trajectory_chunks_fts (session_trajectory_chunks_fts, rank)
             SELECT 'automerge', ?1
             WHERE NOT EXISTS (
                 SELECT 1 FROM session_trajectory_chunks_fts_config
                 WHERE k = 'automerge' AND v = ?1
             )",
            params![FTS_AUTOMERGE_SEGMENTS],
        )
        .map_err(database_error)?;

    chunk_text_kept_with_trajectories(transaction)
}

/// Moves text that the oldest stores kept in `session_trajectories` into chunks.
fn chunk_text_kept_with_trajectories(transaction: &Transaction<'_>) -> Result<()> {
    let existing = {
        let mut statement = transaction
            .prepare(
                "SELECT id, redacted_text FROM session_trajectories
                 WHERE redacted_text <> ''
                   AND NOT EXISTS (
                       SELECT 1 FROM session_trajectory_chunk_rows
                       WHERE trajectory_id = session_trajectories.id
                   )",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| StoreError::CorruptStore)?
    };
    for (trajectory_id, text) in existing {
        let content_hash = Sha256::digest(text.as_bytes()).to_vec();
        transaction
            .execute(
                "UPDATE session_trajectories SET content_hash = ?1 WHERE id = ?2",
                params![content_hash, trajectory_id],
            )
            .map_err(database_error)?;
        insert_trajectory_chunks(transaction, &[(trajectory_id, &text)])?;
    }
    transaction
        .execute(
            "UPDATE session_trajectories SET redacted_text = '' WHERE redacted_text <> ''",
            [],
        )
        .map_err(database_error)?;
    Ok(())
}

/// Replaces the chunks of each `(trajectory ID, redacted text)` document. IDs must be distinct.
fn replace_trajectory_chunks(
    transaction: &Transaction<'_>,
    documents: &[(i64, &str)],
) -> Result<()> {
    if documents.is_empty() {
        return Ok(());
    }
    let trajectory_ids = documents.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let trajectory_ids = serde_json::to_string(&trajectory_ids)
        .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
    transaction
        .execute(
            "DELETE FROM session_trajectory_chunk_rows
             WHERE trajectory_id IN (SELECT value FROM json_each(?1))",
            params![trajectory_ids],
        )
        .map_err(database_error)?;
    insert_trajectory_chunks(transaction, documents)
}

/// Rebuilds the full-text index when a chunk that could not be read was removed.
///
/// The index removes a chunk by its text. For a chunk that cannot be read, the tokens stay in
/// the index under the ID of the removed row. `SQLite` can give that ID to the next chunk, and
/// the tokens would then belong to the wrong text. Every insert of chunks runs this check first,
/// in the same transaction, so no chunk takes such an ID before the index is rebuilt from the
/// stored chunks. The rebuild reads every chunk, which is slow, and it only runs after damage.
fn rebuild_full_text_index_after_unreadable_chunk(transaction: &Transaction<'_>) -> Result<()> {
    let unreadable_chunk_removed = transaction
        .query_row("SELECT 1 FROM session_trajectory_chunk_repairs", [], |_| {
            Ok(())
        })
        .optional()
        .map_err(database_error)?
        .is_some();
    if !unreadable_chunk_removed {
        return Ok(());
    }
    transaction
        .execute_batch(
            "INSERT INTO session_trajectory_chunks_fts (session_trajectory_chunks_fts)
             VALUES ('rebuild');
             DELETE FROM session_trajectory_chunk_repairs;",
        )
        .map_err(database_error)
}

/// Inserts the chunks of each `(trajectory ID, redacted text)` document, many rows per statement.
fn insert_trajectory_chunks(
    transaction: &Transaction<'_>,
    documents: &[(i64, &str)],
) -> Result<()> {
    rebuild_full_text_index_after_unreadable_chunk(transaction)?;
    let (mut rows, mut texts) = (Vec::new(), Vec::new());
    for (trajectory_id, text) in documents {
        for (chunk_index, chunk) in utf8_chunks(text, TRAJECTORY_CHUNK_BYTE_LIMIT).enumerate() {
            let chunk_index =
                i64::try_from(chunk_index).map_err(|_| StoreError::InvalidSessionReference)?;
            rows.push((*trajectory_id, chunk_index));
            texts.push(chunk);
        }
    }
    let rows = rows
        .into_iter()
        .zip(encode_chunks(&texts))
        .map(|((trajectory_id, chunk_index), chunk)| (trajectory_id, chunk_index, chunk))
        .collect::<Vec<_>>();
    for group in rows.chunks(TRAJECTORY_CHUNK_INSERT_ROWS) {
        let mut sql = String::from(
            "INSERT INTO session_trajectory_chunk_rows
             (trajectory_id, chunk_index, stored_text) VALUES ",
        );
        for row in 0..group.len() {
            sql.push_str(if row == 0 { "(?, ?, ?)" } else { ", (?, ?, ?)" });
        }
        let mut statement = transaction.prepare_cached(&sql).map_err(database_error)?;
        for (row, (trajectory_id, chunk_index, chunk)) in group.iter().enumerate() {
            let parameter = row * 3;
            statement
                .raw_bind_parameter(parameter + 1, trajectory_id)
                .and_then(|()| statement.raw_bind_parameter(parameter + 2, chunk_index))
                .and_then(|()| match chunk {
                    StoredChunk::Plain(text) => statement.raw_bind_parameter(parameter + 3, text),
                    StoredChunk::Lz4(stored) => {
                        statement.raw_bind_parameter(parameter + 3, stored.as_slice())
                    }
                })
                .map_err(database_error)?;
        }
        statement.raw_execute().map_err(database_error)?;
    }
    Ok(())
}

fn utf8_chunks(value: &str, byte_limit: usize) -> impl Iterator<Item = &str> {
    let mut offset = 0_usize;
    std::iter::from_fn(move || {
        if offset == value.len() {
            return None;
        }
        let mut end = offset.saturating_add(byte_limit).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let minimum_end = offset.saturating_add(byte_limit / 2).min(value.len());
        if end < value.len()
            && let Some(separator) = value[offset..end].rfind("\n\n")
        {
            let event_end = offset + separator + 2;
            if event_end >= minimum_end {
                end = event_end;
            }
        }
        let chunk = &value[offset..end];
        if end == value.len() {
            offset = end;
        } else {
            // The next chunk repeats the last `SEARCH_QUERY_MAX_CHARS` characters of this one.
            offset = chunk
                .char_indices()
                .rev()
                .nth(SEARCH_QUERY_MAX_CHARS - 1)
                .map(|(index, _)| offset + index)
                .filter(|next| *next > offset)
                .unwrap_or(end);
        }
        Some(chunk)
    })
}

fn insert_handoff(
    transaction: &Transaction<'_>,
    source: &SessionRef,
    target: &SessionRef,
    mode: TransferMode,
    fidelity_json: &str,
    created_at: i64,
) -> Result<()> {
    transaction
        .execute(
            "
            INSERT INTO handoffs (
                source_provider, source_session_id, target_provider, target_session_id,
                mode, fidelity_json, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                source.provider.to_string(),
                source.id,
                target.provider.to_string(),
                target.id,
                transfer_mode_name(mode),
                fidelity_json,
                created_at,
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn replace_branch_head(
    transaction: &Transaction<'_>,
    task_id: i64,
    branch_name: &str,
    session: &SessionRef,
    bound_at: i64,
) -> Result<i64> {
    transaction
        .execute(
            "
            UPDATE session_bindings
            SET is_current = 0
            WHERE task_id = ?1 AND branch_name = ?2 AND is_current = 1
            ",
            params![task_id, branch_name],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "
            INSERT INTO session_bindings
                (task_id, branch_name, provider, session_id, bound_at, is_current)
            VALUES (?1, ?2, ?3, ?4, ?5, 1)
            ",
            params![
                task_id,
                branch_name,
                session.provider.to_string(),
                session.id,
                bound_at,
            ],
        )
        .map_err(database_error)?;
    Ok(transaction.last_insert_rowid())
}

fn query_task(
    connection: &Connection,
    workspace_root: &str,
    name: &str,
) -> Result<Option<TaskRecord>> {
    connection
        .query_row(
            "
            SELECT id, name, workspace_root, created_at
            FROM tasks
            WHERE workspace_root = ?1 AND name = ?2
            ",
            params![workspace_root, name],
            task_from_row,
        )
        .optional()
        .map_err(database_error)
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let timestamp = row.get::<_, i64>(3)?;
    let created_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(TaskRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
        created_at,
    })
}

fn binding_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRecord> {
    let provider = row
        .get::<_, String>(3)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
        })?;
    let timestamp = row.get::<_, i64>(5)?;
    let bound_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(BindingRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        branch_name: row.get(2)?,
        session: SessionRef::new(provider, row.get::<_, String>(4)?),
        bound_at,
        is_current: row.get::<_, i64>(6)? != 0,
    })
}

fn handoff_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffRecord> {
    let source_provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let target_provider = row
        .get::<_, String>(2)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
        })?;
    let mode_name = row.get::<_, String>(4)?;
    let mode = transfer_mode_from_name(&mode_name).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(4, Type::Text, "invalid transfer mode".into())
    })?;
    let timestamp = row.get::<_, i64>(5)?;
    let created_at = DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(5, Type::Integer, "invalid timestamp".into())
    })?;
    Ok(HandoffRecord {
        source: SessionRef::new(source_provider, row.get::<_, String>(1)?),
        target: SessionRef::new(target_provider, row.get::<_, String>(3)?),
        mode,
        created_at,
    })
}

fn catalog_submillis(session: &IndexedSession) -> Option<u32> {
    session
        .updated_at
        .filter(|_| {
            session.session.provider == Provider::OhMyPi
                || (session.session.provider == Provider::Pi && session.source_revision.is_some())
        })
        .map(|time| time.timestamp_subsec_nanos() % 1_000_000)
}

fn prepare_indexed_session(connection: &Connection) -> Result<rusqlite::CachedStatement<'_>> {
    connection.prepare_cached(
        "INSERT INTO session_index (
            provider, session_id, title, project_path, git_branch, created_at, updated_at,
            updated_at_approximate, event_count, indexed_at, source_updated_submillis,
            source_file_id, source_contents_id
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT (provider, session_id) DO UPDATE SET
            title = excluded.title, project_path = excluded.project_path,
            git_branch = excluded.git_branch, created_at = excluded.created_at,
            updated_at = excluded.updated_at, updated_at_approximate = excluded.updated_at_approximate,
            event_count = excluded.event_count, indexed_at = excluded.indexed_at,
            source_updated_submillis = excluded.source_updated_submillis,
            source_file_id = excluded.source_file_id, source_contents_id = excluded.source_contents_id
         WHERE ?14 OR excluded.provider NOT IN ('pi','omp') OR (
            (excluded.source_file_id IS NOT NULL OR session_index.source_file_id IS NULL)
            AND (session_index.source_file_id IS NOT NULL OR NOT EXISTS (
                SELECT 1 FROM session_trajectories AS doc
                WHERE doc.provider = excluded.provider AND doc.session_id = excluded.session_id
                    AND doc.origin = 'native' AND doc.source_complete AND doc.source_file_id IS NULL)))"
    ).map_err(database_error)
}

fn write_indexed_session_row(
    statement: &mut rusqlite::Statement<'_>,
    session: &IndexedSession,
    indexed_at: i64,
    verified: bool,
) -> Result<()> {
    let project = session
        .project_path
        .as_deref()
        .map(|path| path.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
        .transpose()?;
    let count =
        i64::try_from(session.event_count).map_err(|_| StoreError::InvalidSessionReference)?;
    statement
        .execute(params![
            session.session.provider.to_string(),
            session.session.id,
            session.title,
            project,
            session.git_branch,
            session.created_at.map(|t| t.timestamp_millis()),
            session.updated_at.map(|t| t.timestamp_millis()),
            session.updated_at_approximate,
            count,
            indexed_at,
            catalog_submillis(session),
            session
                .source_revision
                .and_then(|r| r.file)
                .map(Uuid::into_bytes),
            session
                .source_revision
                .and_then(|r| r.contents)
                .map(Uuid::into_bytes),
            verified,
        ])
        .map_err(database_error)?;
    Ok(())
}

fn state_from_row(
    row: &rusqlite::Row<'_>,
    column: usize,
    provider: Provider,
) -> rusqlite::Result<TrajectoryIndexState> {
    let remainder: Option<u32> = row.get(column + 3)?;
    let time: i64 = row.get(column)?;
    let source_updated_at = source_time_from_db(time, remainder).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(error))
    })?;
    Ok(TrajectoryIndexState {
        jsonl_tail_reader_version: if samples_jsonl_tail(provider) {
            row.get(column + 9)?
        } else {
            None
        },
        source_revision: source_revision_from_row(row, column + 6)?,
        source_updated_at,
        source_complete: row.get(column + 1)?,
        document_version: row.get(column + 2)?,
        source_time_precise: remainder.is_some(),
        provider_limited: row.get(column + 4)?,
        title_authoritative: row.get(column + 5)?,
    })
}

fn failure_from_row(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<TrajectoryIndexFailure> {
    let remainder = row.get::<_, Option<u32>>(column + 2)?;
    let source_updated_at = row
        .get::<_, Option<i64>>(column)?
        .map(|time| source_time_from_db(time, remainder))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(error))
        })?;
    Ok(TrajectoryIndexFailure {
        source_updated_at,
        source_time_precise: remainder.is_some(),
        document_version: row.get(column + 1)?,
        source_file: optional_uuid(row, column + 3)?,
        source_contents: optional_uuid(row, column + 4)?,
    })
}

fn optional_uuid(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<Uuid>> {
    row.get::<_, Option<[u8; 16]>>(column)
        .map(|id| id.map(Uuid::from_bytes))
}

fn indexed_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSession> {
    let provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let created_at = optional_timestamp_from_row(row, 5)?;
    let remainder = row.get::<_, Option<u32>>(9)?.unwrap_or(0);
    let updated_at = optional_timestamp_from_row(row, 6)?
        .map(|time| time + chrono::Duration::nanoseconds(i64::from(remainder)));
    let updated_at_approximate = row.get(7)?;
    let event_count = row.get::<_, i64>(8)?;
    let event_count = usize::try_from(event_count).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(error))
    })?;
    let file = optional_uuid(row, 10)?;
    let contents = optional_uuid(row, 11)?;
    Ok(IndexedSession {
        session: SessionRef::new(provider, row.get::<_, String>(1)?),
        source_revision: file.map(|file| SourceRevision {
            file: Some(file),
            contents,
            missing_ancestor: None,
        }),
        title: row.get(2)?,
        project_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
        git_branch: row.get(4)?,
        created_at,
        updated_at,
        updated_at_approximate,
        event_count,
    })
}

fn valid_truncation_strategy(strategy: &str) -> bool {
    matches!(
        strategy,
        "none"
            | "source_incomplete"
            | "event_head_tail"
            | "document_head_tail"
            | "event_and_document_head_tail"
            | "source_incomplete_event_head_tail"
            | "source_incomplete_document_head_tail"
            | "source_incomplete_event_and_document_head_tail"
            | "legacy_bounded"
            | "legacy_unknown"
    )
}

fn valid_trajectory_coverage(
    source_byte_count: usize,
    indexed_byte_count: usize,
    strategy: &str,
) -> bool {
    if source_byte_count < indexed_byte_count {
        return false;
    }
    match strategy {
        "none" | "source_incomplete" => source_byte_count == indexed_byte_count,
        "legacy_bounded" | "legacy_unknown" => true,
        _ => source_byte_count > indexed_byte_count,
    }
}

/// One full-text clause. A session matches when every clause matches one of its chunks.
struct TrajectoryClause {
    /// FTS5 phrase of letter-and-digit tokens only, so input never changes query structure.
    match_expression: String,
    /// Folded quoted phrase the matching chunk must contain.
    exact_phrase: Option<String>,
}

fn trajectory_clauses(query: &str) -> Vec<TrajectoryClause> {
    SearchQuery::parse(query)
        .terms()
        .iter()
        .filter_map(trajectory_clause)
        .collect()
}

/// A term's tokens must be adjacent, so `qwen3.8` never matches `qwen3` and `8` far apart. The last
/// token is a prefix when the term ends in a letter or digit, which keeps type-ahead working and
/// lets `"qwen3.8"` find `qwen3.85`, as a substring match would.
fn trajectory_clause(term: &SearchTerm) -> Option<TrajectoryClause> {
    let mut phrase = String::new();
    let mut prefix = term.text().ends_with(char::is_alphanumeric);
    for (index, token) in term.tokens().enumerate() {
        if index == TRAJECTORY_PHRASE_MAX_TOKENS {
            // Punctuation follows the last kept token, so it is a whole token.
            prefix = false;
            break;
        }
        let bounded = utf8_prefix(token, TRAJECTORY_QUERY_MAX_TOKEN_BYTES);
        if !phrase.is_empty() {
            phrase.push(' ');
        }
        phrase.push_str(bounded);
        if bounded.len() < token.len() {
            // A cut token only matches as a prefix, so the phrase ends there.
            prefix = true;
            break;
        }
    }
    if phrase.is_empty() {
        return None;
    }
    Some(TrajectoryClause {
        match_expression: format!("\"{phrase}\"{}", if prefix { "*" } else { "" }),
        exact_phrase: matches!(term, SearchTerm::Exact(_))
            .then(|| search_query::fold_text(term.text())),
    })
}

fn utf8_prefix(value: &str, byte_limit: usize) -> &str {
    if value.len() <= byte_limit {
        return value;
    }
    let mut end = byte_limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

thread_local! {
    /// Buffer that the SQL functions decode compressed chunks into, one after the other.
    static CHUNK_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Registers the folded substring check for exact phrases and the chunk text function. The check
/// folds like the picker's metadata matcher, with [`search_query::fold_text`], so both agree on
/// case; `SQLite`'s `lower()` only folds ASCII.
fn register_search_functions(connection: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    connection.create_scalar_function(FOLDED_CONTAINS_FUNCTION, 2, flags, |context| {
        let folded_phrase = context.get_raw(1).as_str().unwrap_or_default();
        Ok(CHUNK_SCRATCH.with_borrow_mut(|scratch| {
            let text = chunk_text(context.get_raw(0), scratch).unwrap_or_default();
            search_query::contains_folded(text, folded_phrase)
        }))
    })?;
    // The chunk view and the chunk triggers call this function. `SQLite` runs a function from the
    // schema only when the function is marked harmless or the connection trusts the schema.
    let schema_flags = flags | FunctionFlags::SQLITE_INNOCUOUS;
    connection.create_scalar_function(CHUNK_TEXT_FUNCTION, 1, schema_flags, |context| {
        // A chunk that cannot be read, because the file was damaged, gives empty text. An error
        // here would fail every later delete of that chunk, and with it each new index pass of
        // its session and each refresh that prunes the session. The delete goes through instead,
        // and a trigger records that the index still holds the tokens of the chunk.
        Ok(CHUNK_SCRATCH.with_borrow_mut(|scratch| {
            chunk_text(context.get_raw(0), scratch)
                .unwrap_or_default()
                .to_owned()
        }))
    })?;
    connection.create_scalar_function(CHUNK_READABLE_FUNCTION, 1, schema_flags, |context| {
        Ok(CHUNK_SCRATCH
            .with_borrow_mut(|scratch| chunk_text(context.get_raw(0), scratch).is_some()))
    })
}

/// Excerpt around the first occurrence of `folded_phrase`, cut at whitespace, with ` … ` where
/// the chunk continues, like FTS5 `snippet()`.
fn exact_phrase_snippet(text: &str, folded_phrase: &str) -> String {
    let Some(found) = search_query::find_folded(text, folded_phrase) else {
        return String::new();
    };
    let mut start = found.start.saturating_sub(EXACT_SNIPPET_LEADING_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if start > 0
        && let Some(offset) = text[start..found.start].find(char::is_whitespace)
    {
        start += offset;
    }
    let mut end = found
        .end
        .saturating_add(EXACT_SNIPPET_TRAILING_BYTES)
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len()
        && let Some(offset) = text[found.end..end].rfind(char::is_whitespace)
    {
        end = found.end + offset;
    }
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str(" … ");
    }
    snippet.push_str(text[start..end].trim());
    if end < text.len() {
        snippet.push_str(" … ");
    }
    snippet
}

fn optional_timestamp_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<i64>>(index)?
        .map(|timestamp| {
            DateTime::from_timestamp_millis(timestamp).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    Type::Integer,
                    "invalid timestamp".into(),
                )
            })
        })
        .transpose()
}

fn workspace_root_to_string(workspace_root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_owned());
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or(StoreError::InvalidWorkspaceRoot)
}

/// Returns configured local state root without creating it.
///
/// # Errors
///
/// Returns an error when neither `OMNISESSION_HOME` nor a home directory is available.
pub fn state_root() -> Result<PathBuf> {
    if let Some(state_root) = std::env::var_os("OMNISESSION_HOME").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(state_root));
    }
    let base_dirs = BaseDirs::new().ok_or(StoreError::DefaultDirectoryUnavailable)?;
    Ok(base_dirs.home_dir().join(".omnisession"))
}

fn timestamp_from_db(timestamp: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(timestamp).ok_or(StoreError::CorruptStore)
}

fn source_submillis(provider: Provider, time: DateTime<Utc>) -> u32 {
    if TrajectorySourceGuard::uses_file_time(provider) {
        time.timestamp_subsec_nanos() % 1_000_000
    } else {
        0
    }
}

fn source_time_from_db(milliseconds: i64, remainder: Option<u32>) -> Result<DateTime<Utc>> {
    let remainder = remainder.unwrap_or_default();
    if remainder >= 1_000_000 {
        return Err(StoreError::CorruptStore);
    }
    timestamp_from_db(milliseconds)?
        .checked_add_signed(chrono::Duration::nanoseconds(i64::from(remainder)))
        .ok_or(StoreError::CorruptStore)
}

fn now_timestamp() -> i64 {
    Utc::now().timestamp_millis()
}

fn validate_session_ref(session: &SessionRef) -> Result<()> {
    if session.id.is_empty() {
        return Err(StoreError::InvalidSessionReference);
    }
    Ok(())
}

const fn transfer_mode_name(mode: TransferMode) -> &'static str {
    match mode {
        TransferMode::NativeResume => "native_resume",
        TransferMode::OpenWorkspace => "open_workspace",
        TransferMode::NativeFork => "native_fork",
        TransferMode::OfficialImport => "official_import",
        TransferMode::NativeMaterialization => "native_materialization",
        TransferMode::SemanticHandoff => "semantic_handoff",
        TransferMode::PortableExport => "portable_export",
    }
}

fn transfer_mode_from_name(value: &str) -> Option<TransferMode> {
    match value {
        "native_resume" => Some(TransferMode::NativeResume),
        "open_workspace" => Some(TransferMode::OpenWorkspace),
        "native_fork" => Some(TransferMode::NativeFork),
        "official_import" => Some(TransferMode::OfficialImport),
        "native_materialization" => Some(TransferMode::NativeMaterialization),
        "semantic_handoff" => Some(TransferMode::SemanticHandoff),
        "portable_export" => Some(TransferMode::PortableExport),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fmt::Write as _,
        path::{Path, PathBuf},
    };

    use chrono::{DateTime, Utc};
    use directories::BaseDirs;
    use omnis_ir::{
        BundleManifest, CanonicalSnapshot, GitState, PortableBundle, Provider, SCHEMA_VERSION,
        SessionRef, SourceRevision, TransferMode, WorkspaceSnapshot,
    };
    use rusqlite::{params, types::ValueRef};
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{
        BranchHeadRestore, IndexedSession, SessionTrajectoryOrigin, Store,
        TRAJECTORY_CHUNK_BYTE_LIMIT, TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES,
        TRAJECTORY_PHRASE_MAX_TOKENS, TRAJECTORY_QUERY_MAX_TOKEN_BYTES,
        TRAJECTORY_SEARCH_RESULT_LIMIT, TrajectoryDocument, state_root, trajectory_clauses,
        utf8_chunks,
    };
    use super::{CHUNK_HEADER_BYTES, StoreError, StoredChunk, chunk_text, encode_chunk};
    use crate::search_query::SEARCH_QUERY_MAX_CHARS;

    #[test]
    fn task_selection_is_scoped_to_its_workspace() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let first_workspace = temporary_directory.path().join("first");
        let second_workspace = temporary_directory.path().join("second");

        let first = store
            .create_or_get_task("migration", &first_workspace)
            .expect("first task");
        let second = store
            .create_or_get_task("migration", &second_workspace)
            .expect("second task");
        assert_ne!(first.id, second.id);
        assert!(
            store
                .selected_task(&first_workspace)
                .expect("selection lookup")
                .is_none()
        );

        assert_eq!(
            store
                .select_task(&first_workspace, "migration")
                .expect("select first"),
            first
        );
        assert_eq!(
            store
                .selected_task(&first_workspace)
                .expect("first selection"),
            Some(first)
        );
        assert!(
            store
                .selected_task(&second_workspace)
                .expect("second selection")
                .is_none()
        );
    }

    #[test]
    fn session_index_replaces_only_refreshed_provider() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let first_claude = indexed_session(Provider::Claude, "claude-old", "Old title");
        let codex = indexed_session(Provider::Codex, "codex-session", "Codex title");
        store
            .replace_indexed_sessions(Provider::Claude, &[first_claude])
            .expect("index Claude");
        store
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&codex))
            .expect("index Codex");

        let current_claude = indexed_session(Provider::Claude, "claude-new", "New title");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&current_claude))
            .expect("replace Claude index");
        let indexed = store.indexed_sessions().expect("read index");

        assert_eq!(indexed.len(), 2);
        assert!(indexed.contains(&current_claude));
        assert!(indexed.contains(&codex));
        assert!(
            indexed
                .iter()
                .all(|session| session.session.id != "claude-old")
        );
        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::Claude)
                .expect("read Claude index"),
            vec![current_claude]
        );
    }

    #[test]
    fn concurrent_connections_index_upsert_and_search_consistently() {
        const PROVIDERS: [Provider; 4] = [
            Provider::Claude,
            Provider::Codex,
            Provider::OpenCode,
            Provider::Pi,
        ];
        const ROUNDS: i64 = 12;
        let temporary_directory = tempdir().expect("temporary directory");
        let store_path = temporary_directory.path().join("store.sqlite3");
        drop(Store::open(&store_path).expect("initialize store"));
        let source_updated_at = Utc::now();
        let barrier = std::sync::Barrier::new(PROVIDERS.len());

        std::thread::scope(|scope| {
            for provider in PROVIDERS {
                let (store_path, barrier) = (&store_path, &barrier);
                scope.spawn(move || {
                    let store = Store::open(store_path).expect("open concurrent connection");
                    barrier.wait();
                    for round in 0..ROUNDS {
                        let sessions = [
                            indexed_session(provider, "stable", "Stable title"),
                            indexed_session(provider, &format!("round-{round}"), "Round title"),
                        ];
                        store
                            .replace_indexed_sessions(provider, &sessions)
                            .expect("replace provider index concurrently");
                        for session in &sessions {
                            let text = format!("sharedneedle generation{round}");
                            store
                                .upsert_session_trajectory_document(
                                    &session.session,
                                    &text,
                                    source_updated_at + chrono::Duration::seconds(round),
                                    text.len(),
                                    text.len(),
                                    "none",
                                    true,
                                    SessionTrajectoryOrigin::Native,
                                )
                                .expect("upsert trajectory concurrently");
                        }
                        let own_matches = store
                            .search_session_trajectory_page("sharedneedle", 512)
                            .expect("search during concurrent writes")
                            .matches
                            .into_iter()
                            .filter(|item| item.session.provider == provider)
                            .map(|item| item.session)
                            .collect::<HashSet<_>>();
                        assert_eq!(
                            own_matches,
                            sessions
                                .iter()
                                .map(|session| session.session.clone())
                                .collect::<HashSet<_>>(),
                            "{provider} search must see its own committed round"
                        );
                    }
                });
            }
        });

        let store = Store::open(&store_path).expect("reopen store");
        let final_round = ROUNDS - 1;
        let expected = PROVIDERS
            .iter()
            .flat_map(|provider| {
                [
                    SessionRef::new(*provider, "stable"),
                    SessionRef::new(*provider, format!("round-{final_round}")),
                ]
            })
            .collect::<HashSet<_>>();
        let indexed = store
            .indexed_sessions()
            .expect("final index")
            .into_iter()
            .map(|session| session.session)
            .collect::<Vec<_>>();
        assert_eq!(indexed.len(), expected.len());
        assert_eq!(indexed.into_iter().collect::<HashSet<_>>(), expected);
        let search = |query: &str| {
            store
                .search_session_trajectories(query, 512)
                .expect("final search")
                .into_iter()
                .collect::<HashSet<_>>()
        };
        assert_eq!(
            search("sharedneedle"),
            expected,
            "stale rounds were not pruned"
        );
        assert_eq!(search(&format!("generation{final_round}")), expected);
        assert!(search(&format!("generation{}", final_round - 1)).is_empty());
    }

    #[test]
    fn delayed_catalog_cannot_replace_or_prune_a_verified_native_session() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("index.sqlite3");
        let discovery = Store::open(&path).unwrap();
        let show = Store::open(&path).unwrap();
        let time = DateTime::from_timestamp(1000, 43).unwrap();
        for provider in [Provider::Pi, Provider::OhMyPi] {
            let observe = || {
                discovery
                    .observe_session_catalog(provider)
                    .unwrap()
                    .unwrap()
            };
            let empty = observe();
            let rev = SourceRevision {
                file: Some(Uuid::from_u128(1)),
                contents: Some(Uuid::from_u128(2)),
                missing_ancestor: None,
            };
            let mut previous = indexed_session(provider, "selected", "Earlier");
            previous.source_revision = Some(rev);
            previous.updated_at = Some(time);
            let session = &previous.session;
            let publish = |metadata: &IndexedSession, text| {
                let revision = metadata.source_revision.unwrap();
                show.upsert_verified_session_document_checked(
                    metadata,
                    physical_document(text, time, revision),
                    show.observe_verified_session(session).unwrap(),
                    |_, actual| actual == revision,
                )
                .unwrap();
            };
            publish(&previous, "archived");
            let older = observe();
            let replaced = IndexedSession {
                source_revision: Some(SourceRevision {
                    contents: Some(Uuid::from_u128(3)),
                    ..rev
                }),
                title: Some("Verified".to_owned()),
                project_path: Some("/workspace/current".into()),
                ..previous.clone()
            };
            publish(&replaced, "currentbody");
            for complete in [true, false] {
                assert!(matches!(
                    discovery.refresh_observed_indexed_sessions(
                        older,
                        std::slice::from_ref(&previous),
                        complete,
                        |_, _| true
                    ),
                    Err(StoreError::TrajectorySourceChanged)
                ));
            }
            assert!(matches!(
                discovery.refresh_observed_indexed_sessions(empty, &[], true, |_, _| true),
                Err(StoreError::TrajectorySourceChanged)
            ));
            let current = observe();
            discovery
                .refresh_observed_indexed_sessions(current, &[], false, |_, _| true)
                .unwrap();
            assert_eq!(
                discovery.indexed_sessions_for_provider(provider).unwrap(),
                std::slice::from_ref(&replaced)
            );
            assert!(
                discovery
                    .search_session_trajectories("currentbody", 2)
                    .unwrap()
                    .contains(session)
            );
            discovery
                .refresh_observed_indexed_sessions(current, &[], true, |_, _| true)
                .unwrap();
            assert!(
                discovery
                    .indexed_sessions_for_provider(provider)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                !discovery
                    .search_session_trajectories("currentbody", 2)
                    .unwrap()
                    .contains(session)
            );
        }
    }

    fn discovered_file(id: &str, serial: u128) -> IndexedSession {
        let mut row = indexed_session(Provider::Pi, id, id);
        row.source_revision = Some(SourceRevision {
            file: Some(Uuid::from_u128(serial)),
            contents: Some(Uuid::from_u128(serial + 1)),
            missing_ancestor: None,
        });
        row
    }

    #[test]
    fn discovery_preserves_concurrent_physical_failures_and_merges_unchanged_peers() {
        let temporary = tempdir().unwrap();
        let db = temporary.path().join("state.sqlite");
        let scan = Store::open(&db).unwrap();
        let show = Store::open(&db).unwrap();
        for provider in [Provider::Pi, Provider::OhMyPi] {
            let listed = [("selected", 10), ("peer", 20)].map(|(id, serial)| {
                let mut row = discovered_file(id, serial);
                row.session.provider = provider;
                row
            });
            let session = &listed[0].session;
            let observe = || scan.observe_session_catalog(provider).unwrap().unwrap();
            let first = observe();
            let mut failure = super::TrajectoryIndexFailure {
                source_file: Some(Uuid::from_u128(42)),
                source_contents: Some(Uuid::from_u128(43)),
                source_updated_at: Some(DateTime::from_timestamp(1000, 9).unwrap()),
                source_time_precise: true,
                document_version: 6,
            };
            show.record_trajectory_index_failure(session, &failure)
                .unwrap();
            assert!(matches!(
                scan.refresh_observed_indexed_sessions(first, &listed, true, |_, _| true),
                Err(StoreError::TrajectorySourceChanged)
            ));
            let retry = scan.observe_catalog_retry(first, &listed).unwrap();
            show.record_trajectory_index_failure(session, &failure)
                .unwrap();
            assert_eq!(retry.observation(), observe());
            failure.source_contents = Some(Uuid::from_u128(44));
            show.record_trajectory_index_failure(session, &failure)
                .unwrap();
            assert_eq!(
                scan.refresh_retried_indexed_sessions(&retry, &listed, true, |_, _| true)
                    .unwrap(),
                Some(vec![1])
            );
            assert_eq!(
                scan.indexed_sessions_for_provider(provider).unwrap(),
                listed[1..]
            );
            assert_eq!(
                scan.trajectory_index_failures().unwrap().get(session),
                Some(&failure)
            );
            let retry = scan.observe_catalog_retry(observe(), &listed).unwrap();
            show.clear_observed_trajectory_index_failure(session, &failure)
                .unwrap();
            assert_eq!(
                scan.refresh_retried_indexed_sessions(&retry, &listed, true, |_, _| true)
                    .unwrap(),
                Some(vec![1])
            );
            assert!(scan.session_index_refreshed_at(provider).unwrap().is_none());
        }
    }

    #[test]
    fn index_file_observation_checks_each_saved_physical_source() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("state.sqlite")).unwrap();
        let row = discovered_file("selected", 1);
        let session = &row.session;
        let revision = row.source_revision.unwrap();
        let file = revision.file.unwrap();
        let rival = Uuid::from_u128(3);
        let observe = |file, disputed| {
            let (guard, conflict) = store.observe_index_file(session, file).unwrap();
            assert_eq!(conflict, disputed);
            assert_eq!(guard, store.observe_verified_session(session).unwrap());
            guard
        };
        let empty = observe(file, false);
        let mut failure = super::TrajectoryIndexFailure {
            source_file: Some(rival),
            source_contents: None,
            source_updated_at: None,
            source_time_precise: false,
            document_version: 6,
        };
        store
            .record_trajectory_index_failure(session, &failure)
            .unwrap();
        assert_ne!(observe(file, false), empty);
        failure.source_contents = Some(Uuid::from_u128(4));
        store
            .record_trajectory_index_failure(session, &failure)
            .unwrap();
        observe(file, true);
        store.clear_trajectory_index_failure(session).unwrap();
        store.upsert_indexed_session(&row).unwrap();
        observe(file, false);
        observe(rival, true);
        let rival_revision = SourceRevision {
            file: Some(rival),
            ..revision
        };
        store
            .upsert_trajectory_document(
                session,
                &physical_document("rival", Utc::now(), rival_revision),
            )
            .unwrap();
        observe(file, true);
        observe(rival, true);
    }

    #[test]
    fn repeating_discovery_merges_stable_files_without_replacing_contested_rows() {
        let temporary = tempdir().unwrap();
        let db = temporary.path().join("state.sqlite");
        let scan = Store::open(&db).unwrap();
        let other = Store::open(&db).unwrap();
        let provider = Provider::Pi;
        let archived = discovered_file("archived", 5);
        let stable = discovered_file("stable", 10);
        let hot = discovered_file("active", 20);
        let twin = discovered_file("duplicate", 30);
        let other_twin = discovered_file("duplicate", 40);
        let moved = discovered_file("moved", 50);
        let latest = discovered_file("moved", 60);
        let first = [
            stable.clone(),
            hot.clone(),
            twin.clone(),
            other_twin.clone(),
            moved.clone(),
        ];
        scan.replace_indexed_sessions(provider, std::slice::from_ref(&archived))
            .unwrap();
        let refresh = scan.session_index_refreshed_at(provider).unwrap();
        let checked = scan.session_index_checked_at(provider).unwrap();
        let observed = scan.observe_session_catalog(provider).unwrap().unwrap();
        assert!(matches!(
            scan.refresh_observed_indexed_sessions(observed, &first, false, |_, _| true),
            Err(StoreError::TrajectorySourceChanged)
        ));
        let retry = scan.observe_catalog_retry(observed, &first).unwrap();
        other.upsert_indexed_session(&other_twin).unwrap();
        other.upsert_indexed_session(&latest).unwrap();
        let second = [stable.clone(), hot, twin, moved];
        let accepted = scan
            .refresh_retried_indexed_sessions(&retry, &second, true, |index, _| index != 1)
            .unwrap();
        assert_eq!(accepted, Some(vec![0]));
        let stored = scan.indexed_sessions_for_provider(provider).unwrap();
        assert_eq!(stored.len(), 4);
        for expected in [archived, stable, other_twin, latest] {
            assert!(stored.contains(&expected));
        }
        assert_eq!(scan.session_index_refreshed_at(provider).unwrap(), refresh);
        assert_eq!(scan.session_index_checked_at(provider).unwrap(), checked);
    }

    #[test]
    fn retry_accounts_for_missing_files_and_concurrent_membership_before_marking_fresh() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("state.sqlite")).unwrap();
        let provider = Provider::Pi;
        let stable = discovered_file("stable", 100);
        let missing = discovered_file("missing", 200);
        let concurrent = discovered_file("other", 300);
        let listed = std::slice::from_ref(&stable);
        let observe = |rows: &[IndexedSession]| {
            let prior = store.observe_session_catalog(provider).unwrap().unwrap();
            store.observe_catalog_retry(prior, rows).unwrap()
        };
        let partial = observe(&[stable.clone(), missing]);
        assert_eq!(
            store
                .refresh_retried_indexed_sessions(&partial, listed, false, |_, _| true)
                .unwrap(),
            Some(vec![0])
        );
        assert!(store.session_index_checked_at(provider).unwrap().is_none());
        let earlier = observe(listed);
        store.upsert_indexed_session(&concurrent).unwrap();
        assert_eq!(
            store
                .refresh_retried_indexed_sessions(&earlier, listed, true, |_, _| true)
                .unwrap(),
            Some(vec![0])
        );
        assert!(
            store
                .indexed_sessions_for_provider(provider)
                .unwrap()
                .contains(&concurrent)
        );
        assert!(store.session_index_checked_at(provider).unwrap().is_none());
        assert!(
            store
                .session_index_refreshed_at(provider)
                .unwrap()
                .is_none()
        );
        let ready = observe(listed);
        assert_eq!(
            store
                .refresh_retried_indexed_sessions(&ready, listed, true, |_, _| true)
                .unwrap(),
            None
        );
        assert_eq!(
            store.indexed_sessions_for_provider(provider).unwrap(),
            listed
        );
        assert!(
            store
                .session_index_refreshed_at(provider)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn catalog_unchanged_fast_path_still_verifies_files_and_rolls_back() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("index.sqlite3")).unwrap();
        let provider = Provider::Pi;
        let mut entry = indexed_session(provider, "stable", "Fixture");
        entry.source_revision = Some(SourceRevision {
            file: Some(Uuid::from_u128(5)),
            contents: Some(Uuid::from_u128(6)),
            missing_ancestor: None,
        });
        let listed = std::slice::from_ref(&entry);
        store.replace_indexed_sessions(provider, listed).unwrap();
        let observed = store.observe_session_catalog(provider).unwrap().unwrap();
        {
            let db = store.connection.borrow();
            db.execute("UPDATE session_index SET indexed_at=7", [])
                .unwrap();
            db.execute("UPDATE session_index_checks SET checked_at=9", [])
                .unwrap();
        }
        assert_eq!(
            store.observe_session_catalog(provider).unwrap(),
            Some(observed)
        );
        assert!(matches!(
            store.refresh_observed_indexed_sessions(observed, listed, true, |_, _| false),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(
            store
                .session_index_checked_at(provider)
                .unwrap()
                .unwrap()
                .timestamp_millis(),
            9
        );
        store
            .refresh_observed_indexed_sessions(observed, listed, true, |index, receipt| {
                index == 0 && Some(receipt) == entry.source_revision
            })
            .unwrap();
        assert_eq!(
            store.observe_session_catalog(provider).unwrap(),
            Some(observed)
        );
        let stamp: i64 = store
            .connection
            .borrow()
            .query_row("SELECT indexed_at FROM session_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamp, 7);
        assert!(
            store
                .observe_session_catalog(Provider::Claude)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unchanged_provider_refresh_preserves_index_rows() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = indexed_session(Provider::Claude, "stable", "Stable title");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&session))
            .expect("initial refresh");
        {
            let connection = store.connection.borrow();
            connection
                .execute(
                    "UPDATE session_index SET indexed_at = 42
                     WHERE provider = 'claude' AND session_id = 'stable'",
                    [],
                )
                .expect("set sentinel timestamp");
        }

        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&session))
            .expect("unchanged refresh");

        let connection = store.connection.borrow();
        let indexed_at = connection
            .query_row(
                "SELECT indexed_at FROM session_index
                 WHERE provider = 'claude' AND session_id = 'stable'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained row");
        assert_eq!(indexed_at, 42);
    }

    #[test]
    fn forgetting_session_removes_cache_and_active_binding() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let removed = indexed_session(Provider::Codex, "removed", "Removed title");
        let retained = indexed_session(Provider::Codex, "retained", "Retained title");
        store
            .replace_indexed_sessions(Provider::Codex, &[removed.clone(), retained.clone()])
            .expect("index sessions");
        store
            .upsert_session_trajectory(&removed.session, "unique deletion marker", Utc::now(), true)
            .expect("index trajectory");
        let task = store
            .create_or_get_task("deletion", "/workspace")
            .expect("create task");
        store
            .bind_session(task.id, "main", &removed.session)
            .expect("bind removed session");

        store
            .forget_session(&removed.session)
            .expect("forget session");

        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::Codex)
                .expect("remaining sessions"),
            vec![retained]
        );
        assert!(
            store
                .search_session_trajectories("unique deletion marker", 10)
                .expect("search removed trajectory")
                .is_empty()
        );
        assert!(
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .is_none()
        );
    }

    #[test]
    fn restoring_replaced_branch_head_keeps_a_newer_head() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let task = store
            .create_or_get_task("interrupt", "/workspace")
            .expect("create task");
        let source = SessionRef::new(Provider::Claude, "source");
        let newer = SessionRef::new(Provider::Codex, "newer");
        let generated = SessionRef::new(Provider::Pi, "generated");
        let regenerated = SessionRef::new(Provider::Pi, "regenerated");
        let fidelity = serde_json::json!({});
        let head = || {
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .map(|binding| binding.session)
        };
        store
            .bind_session(task.id, "main", &source)
            .expect("bind source");

        // Another command moves the head after the import records its lineage.
        store
            .record_handoff_and_bind(
                task.id,
                "main",
                &source,
                &generated,
                TransferMode::NativeMaterialization,
                &fidelity,
            )
            .expect("record import");
        store
            .bind_session(task.id, "main", &newer)
            .expect("move head");
        assert_eq!(
            store
                .restore_replaced_branch_head(task.id, "main", &generated)
                .expect("undo import"),
            BranchHeadRestore::Moved
        );
        assert_eq!(head(), Some(newer.clone()));

        // An import that replaced the newer head restores it, not the head read before recording.
        store
            .record_handoff_and_bind(
                task.id,
                "main",
                &source,
                &regenerated,
                TransferMode::NativeMaterialization,
                &fidelity,
            )
            .expect("record import over newer head");
        assert_eq!(
            store
                .restore_replaced_branch_head(task.id, "main", &regenerated)
                .expect("undo import"),
            BranchHeadRestore::Restored(Some(newer.clone()))
        );
        assert_eq!(head(), Some(newer));
    }

    #[test]
    fn trajectory_search_supports_words_phrases_and_literal_operator_text() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let exact = SessionRef::new(Provider::Claude, "exact");
        let separated = SessionRef::new(Provider::Codex, "separated");
        let source_updated_at = Utc::now();

        store
            .upsert_session_trajectory(
                &exact,
                "Fixed refresh token race in OAuth worker",
                source_updated_at,
                true,
            )
            .expect("index exact trajectory");
        store
            .upsert_session_trajectory(
                &separated,
                "Refresh stale credentials before token race test",
                source_updated_at,
                true,
            )
            .expect("index separated trajectory");

        let word_matches = store
            .search_session_trajectories("refresh token", 10)
            .expect("word search");
        assert_eq!(word_matches.len(), 2);
        assert!(word_matches.contains(&exact));
        assert!(word_matches.contains(&separated));
        let contextual_matches = store
            .search_session_trajectory_matches("OAuth worker", 10)
            .expect("contextual search");
        assert_eq!(contextual_matches.len(), 1);
        assert_eq!(contextual_matches[0].session, exact);
        assert!(contextual_matches[0].snippet.contains("OAuth worker"));
        assert!(contextual_matches[0].complete);
        assert_eq!(
            contextual_matches[0].indexed_byte_count,
            "Fixed refresh token race in OAuth worker".len()
        );
        assert_eq!(
            store
                .search_session_trajectories("OAuth work", 10)
                .expect("prefix search"),
            vec![exact.clone()]
        );
        assert_eq!(
            store
                .search_session_trajectories("\"refresh token\"", 10)
                .expect("phrase search"),
            vec![exact]
        );
        assert!(
            store
                .search_session_trajectories("refresh\") OR token:*", 10)
                .expect("literal operator search")
                .is_empty()
        );
        assert!(
            store
                .search_session_trajectories("[]{}:*", 10)
                .expect("punctuation-only search")
                .is_empty()
        );
    }

    #[test]
    fn quoted_phrases_match_exact_text_and_joined_words_match_adjacent_tokens() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let filler = "unrelated filler words ".repeat(40);
        for (id, text) in [
            (
                "exact",
                format!("qwen3 8 came first. {filler}Then Qwen3.8-Coder shipped"),
            ),
            ("spaced", "qwen3 8 quantized".to_owned()),
            ("hyphen", "the qwen3-8 variant".to_owned()),
            ("scattered", "qwen3.5 notes after 8 runs".to_owned()),
            ("separate", "qwen3 baseline\n\nretried 8 times".to_owned()),
        ] {
            store
                .upsert_session_trajectory(
                    &SessionRef::new(Provider::Codex, id),
                    &text,
                    Utc::now(),
                    true,
                )
                .expect("trajectory");
        }
        let ids = |query: &str| {
            let mut ids = store
                .search_session_trajectory_matches(query, 10)
                .expect("search")
                .into_iter()
                .map(|item| item.session.id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids
        };

        let exact = store
            .search_session_trajectory_matches("\"qwen3.8\"", 10)
            .expect("exact phrase");
        assert_eq!(
            exact
                .iter()
                .map(|item| item.session.id.as_str())
                .collect::<Vec<_>>(),
            ["exact"]
        );
        // The snippet shows the exact occurrence, not the earlier token-equal "qwen3 8".
        assert!(
            exact[0].snippet.contains("Qwen3.8-Coder"),
            "{}",
            exact[0].snippet
        );
        assert_eq!(ids("\"qwen3.8-coder"), ["exact"]);
        assert_eq!(ids("shipped \"qwen3.8\" first"), ["exact"]);
        assert!(ids("\"qwen3.8\" quantized").is_empty());
        assert!(ids("\"\"").is_empty());
        assert_eq!(ids("qwen3.8"), ["exact", "hyphen", "spaced"]);
    }

    #[test]
    fn trajectory_query_tokens_respect_utf8_byte_limit() {
        let clauses = trajectory_clauses(&"界".repeat(200));
        let expression = &clauses[0].match_expression;
        let token = &expression[1..expression.len() - 2];

        assert!(token.len() <= TRAJECTORY_QUERY_MAX_TOKEN_BYTES);
        assert!(std::str::from_utf8(token.as_bytes()).is_ok());

        let query = format!("{}界b", "a".repeat(255));
        let clauses = trajectory_clauses(&query);
        let expression = &clauses[0].match_expression;
        let token = &expression[1..expression.len() - 2];
        assert_eq!(token, "a".repeat(255));
        assert!(query.starts_with(token));
    }

    #[test]
    fn newer_bounded_source_replaces_older_complete_coverage() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store_path = temporary_directory.path().join("store.sqlite3");
        let session = SessionRef::new(Provider::Codex, "freshness");
        let source_updated_at = Utc::now();
        let store = Store::open(&store_path).expect("store");

        store
            .upsert_session_trajectory_document(
                &session,
                "complete trajectory includes durable-marker",
                source_updated_at,
                "complete trajectory includes durable-marker".len(),
                "complete trajectory includes durable-marker".len(),
                "none",
                true,
                SessionTrajectoryOrigin::Native,
            )
            .expect("index complete trajectory");
        store
            .upsert_session_trajectory_document(
                &session,
                "new bounded trajectory includes transient-marker",
                source_updated_at + chrono::Duration::seconds(1),
                20_000_000,
                "new bounded trajectory includes transient-marker".len(),
                "document_head_tail",
                true,
                SessionTrajectoryOrigin::Native,
            )
            .expect("index newer bounded source");
        drop(store);

        let reopened = Store::open(&store_path).expect("reopen store");
        assert!(
            reopened
                .search_session_trajectories("durable-marker", 10)
                .expect("search replaced trajectory")
                .is_empty()
        );
        let matches = reopened
            .search_session_trajectory_matches("transient-marker", 10)
            .expect("search newer bounded source");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session, session);
        assert!(matches[0].source_complete);
        assert!(!matches[0].complete);
        assert_eq!(matches[0].truncation_strategy, "document_head_tail");
    }

    #[test]
    fn version_upgrade_preserves_full_source_while_newer_same_version_preview_can_refresh() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Claude, "quality");
        let source_updated_at = Utc::now();
        let full_source = "full source retains durable-edge";
        store
            .upsert_session_trajectory_document(
                &session,
                full_source,
                source_updated_at,
                20_000_000,
                full_source.len(),
                "document_head_tail",
                true,
                SessionTrajectoryOrigin::Native,
            )
            .expect("full source head-tail");
        let preview = "preview-only transient-edge";
        let write_preview = |document_version, source_updated_at| {
            store
                .upsert_trajectory_document(
                    &session,
                    &TrajectoryDocument {
                        source_revision: SourceRevision::default(),
                        redacted_text: preview,
                        source_updated_at,
                        source_byte_count: preview.len(),
                        indexed_byte_count: preview.len(),
                        truncation_strategy: "none",
                        source_complete: false,
                        provider_limited: false,
                        origin: SessionTrajectoryOrigin::Native,
                        document_version,
                        derived_title: None,
                        title_authoritative: false,
                    },
                )
                .expect("write preview");
        };
        let later = source_updated_at + chrono::Duration::milliseconds(1);
        for (version, time) in [(0, source_updated_at), (1, source_updated_at), (1, later)] {
            write_preview(version, time);
        }

        assert_eq!(
            store
                .search_session_trajectories("durable-edge", 10)
                .expect("full source remains"),
            vec![session.clone()]
        );
        assert!(
            store
                .search_session_trajectories("transient-edge", 10)
                .expect("preview excluded")
                .is_empty()
        );
        assert!(
            store
                .session_trajectory_source_is_current(&session, source_updated_at)
                .expect("source coverage")
        );

        write_preview(0, later);
        assert_eq!(
            store
                .search_session_trajectories("transient-edge", 10)
                .expect("newer preview is searchable"),
            [session]
        );
    }

    #[test]
    fn document_versions_gate_replacement_and_report_index_state() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "versioned");
        let source_updated_at = Utc::now();
        let document =
            |text: &'static str,
             document_version: u32,
             derived_title: Option<&'static str>,
             source_updated_at: DateTime<Utc>| TrajectoryDocument {
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at,
                source_byte_count: text.len(),
                indexed_byte_count: text.len(),
                truncation_strategy: "none",
                source_complete: true,
                provider_limited: false,
                origin: SessionTrajectoryOrigin::Native,
                document_version,
                derived_title,
                title_authoritative: false,
            };
        store
            .upsert_session_trajectory(&session, "legacy redaction marker", source_updated_at, true)
            .expect("legacy document");
        assert_eq!(
            store.trajectory_index_states().expect("states")[&session].document_version,
            0
        );

        store
            .upsert_trajectory_document(
                &session,
                &document(
                    "rebuilt marker",
                    2,
                    Some("Synthetic title"),
                    source_updated_at,
                ),
            )
            .expect("newer version");
        let state = store.trajectory_index_states().expect("states")[&session];
        assert_eq!(state.document_version, 2);
        assert!(state.source_complete);
        for (minimum, current) in [(2, true), (3, false)] {
            assert_eq!(
                store
                    .session_trajectory_source_is_current_at_version(
                        &session,
                        source_updated_at,
                        minimum
                    )
                    .unwrap(),
                current
            );
        }
        assert_eq!(
            store
                .search_session_trajectories("rebuilt", 10)
                .expect("search rebuilt"),
            vec![session.clone()]
        );
        assert!(
            store
                .search_session_trajectories("legacy", 10)
                .expect("search legacy")
                .is_empty()
        );

        let later = source_updated_at + chrono::Duration::seconds(5);
        store
            .upsert_trajectory_document(&session, &document("older writer marker", 1, None, later))
            .expect("older version write");
        assert!(
            store
                .search_session_trajectories("older", 10)
                .expect("search older")
                .is_empty()
        );
        store
            .upsert_trajectory_document(&session, &document("newer source marker", 2, None, later))
            .expect("newer source");
        assert_eq!(
            store
                .search_session_trajectories("newer", 10)
                .expect("search newer"),
            vec![session.clone()]
        );
        assert_eq!(
            store
                .trajectory_titles()
                .expect("titles")
                .get(&session)
                .map(String::as_str),
            Some("Synthetic title")
        );
    }

    #[test]
    fn a_provider_limit_can_replace_an_older_full_session_during_upgrade() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Pi, "growing-session");
        let initial = Utc::now();
        let write = |text: &str, seconds, version, full, limited| {
            store
                .upsert_trajectory_document(
                    &session,
                    &TrajectoryDocument {
                        source_revision: SourceRevision::default(),
                        redacted_text: text,
                        source_updated_at: initial + chrono::Duration::seconds(seconds),
                        source_byte_count: text.len(),
                        indexed_byte_count: text.len(),
                        truncation_strategy: "none",
                        source_complete: full,
                        provider_limited: limited,
                        origin: SessionTrajectoryOrigin::Native,
                        document_version: version,
                        derived_title: None,
                        title_authoritative: false,
                    },
                )
                .unwrap();
        };
        write("oldmiddle", 0, 4, true, false);
        write("newpreview", 1, 6, false, false);
        write("retiredtail", -1, 6, false, true);
        assert_eq!(
            store.search_session_trajectories("oldmiddle", 1).unwrap(),
            std::slice::from_ref(&session)
        );
        write("currenttail", 1, 6, false, true);
        write("sameeventpreview", 1, 6, false, false);
        assert_eq!(
            store.search_session_trajectories("currenttail", 1).unwrap(),
            std::slice::from_ref(&session)
        );
        let state = store.trajectory_index_state(&session).unwrap().unwrap();
        assert_eq!(state.document_version, 6);
        assert!(state.provider_limited);
        assert!(!state.source_complete);
    }

    #[test]
    fn verified_full_read_keeps_same_time_search_and_catalog_changes_together() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("index.sqlite3")).unwrap();
        let time = DateTime::from_timestamp(1_000, 843).unwrap();
        let mut catalog = indexed_session(Provider::OhMyPi, "restored-omp", "Initial title");
        catalog.updated_at = Some(time);
        let document = |text: &'static str| TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at: time,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 5,
            derived_title: Some(text),
            title_authoritative: false,
        };
        let old = document("oldmarker");
        let new = TrajectoryDocument {
            source_byte_count: 12_000,
            truncation_strategy: "event_head_tail",
            ..document("newmarker")
        };
        let session = catalog.session.clone();
        let observe = || store.observe_verified_session(&session).unwrap();
        let publish = |meta: &IndexedSession, doc, guard| {
            store.upsert_verified_session_document(meta, doc, guard)
        };
        assert_eq!(publish(&catalog, old, observe()).unwrap(), 1);
        let before = observe();
        catalog.title = Some("Replacement title".to_owned());
        let ordinary = super::TrajectorySourceGuard::Observed(Some(time));
        let sampled = TrajectoryDocument {
            source_complete: false,
            ..new
        };
        let provider_limited = TrajectoryDocument {
            provider_limited: true,
            ..sampled
        };
        for unverified in [sampled, provider_limited, new] {
            let batch = [(&session, unverified, ordinary)];
            let rewritten = store
                .upsert_observed_trajectory_documents_counted(&batch)
                .unwrap();
            assert_eq!(rewritten, 0);
        }
        assert_eq!(publish(&catalog, new, before).unwrap(), 1);
        assert_eq!(publish(&catalog, new, before).unwrap(), 0);
        assert!(matches!(
            publish(&catalog, old, before),
            Err(StoreError::TrajectorySourceChanged)
        ));

        let before_move = observe();
        let mut moved = catalog.clone();
        moved.project_path = Some(PathBuf::from("/workspace/moved"));
        assert_eq!(publish(&moved, new, before_move).unwrap(), 0);
        assert!(matches!(
            publish(&catalog, new, before_move),
            Err(StoreError::TrajectorySourceChanged)
        ));
        let previous_version = TrajectoryDocument {
            document_version: 4,
            ..new
        };
        assert!(matches!(
            publish(&catalog, previous_version, observe()),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(matches!(
            publish(&catalog, sampled, observe()),
            Err(StoreError::InvalidSessionReference)
        ));

        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TRIGGER fail_catalog BEFORE UPDATE ON session_index
             BEGIN SELECT RAISE(ABORT, 'catalog write failed'); END;",
            )
            .unwrap();
        assert!(publish(&catalog, old, observe()).is_err());
        let stored = store.indexed_sessions().unwrap().pop().unwrap();
        assert_eq!(stored.project_path, moved.project_path);
        assert_eq!(stored.title, moved.title);
        assert!(
            store
                .search_session_trajectories("oldmarker", 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search_session_trajectories("newmarker", 1).unwrap(),
            [session]
        );
    }

    #[test]
    fn verified_file_change_advances_identical_document_and_rechecks_unchanged_source() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let mut catalog = indexed_session(Provider::Pi, "restored", "Unchanged title");
        let time = DateTime::from_timestamp(1000, 843).unwrap();
        catalog.updated_at = Some(time);
        let revision = |n| SourceRevision {
            file: Some(Uuid::from_u128(n)),
            contents: Some(Uuid::from_u128(n + 1)),
            missing_ancestor: None,
        };
        let document = |text, file| physical_document(text, time, file);
        let a = document("retained", revision(1));
        let b = document("withdrawn", revision(3));
        let restored = document("retained", revision(5));
        let session = catalog.session.clone();
        let observe = || store.observe_verified_session(&session).unwrap();
        let publish = |doc: TrajectoryDocument<'_>, guard, selected| {
            let meta = IndexedSession {
                source_revision: Some(doc.source_revision),
                ..catalog.clone()
            };
            store.upsert_verified_session_document_checked(&meta, doc, guard, |_, r| r == selected)
        };
        assert_eq!(publish(a, observe(), a.source_revision).unwrap(), 1);
        assert_eq!(store.indexed_sessions().unwrap()[0].updated_at, Some(time));
        let prior = observe();
        assert_eq!(
            publish(restored, prior, restored.source_revision).unwrap(),
            1
        );
        assert_ne!(observe(), prior);
        assert!(matches!(
            publish(b, prior, b.source_revision),
            Err(StoreError::TrajectorySourceChanged)
        ));

        let same_object = observe();
        assert_eq!(
            publish(restored, same_object, restored.source_revision).unwrap(),
            0
        );
        assert_eq!(observe(), same_object);
        // A directory swap can return the original inode with an unchanged cache and file stamp.
        for candidate in [restored, b] {
            let different_file = if candidate.source_revision == b.source_revision {
                restored
            } else {
                b
            };
            assert!(matches!(
                publish(candidate, same_object, different_file.source_revision),
                Err(StoreError::TrajectorySourceChanged)
            ));
        }
        let background = store.observe_trajectory_document(&session).unwrap();
        assert!(matches!(
            store.upsert_observed_trajectory_documents_checked(
                &[(&session, b, background)],
                |_, r| r == restored.source_revision
            ),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(
            store.search_session_trajectories("retained", 1).unwrap(),
            [session]
        );
        assert!(
            store
                .search_session_trajectories("withdrawn", 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_changed_source_rolls_back_its_catalog_and_the_other_documents_in_the_batch() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp(1000, 843).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let mut catalog = indexed_session(Provider::Pi, "selected", "Current catalog");
        catalog.updated_at = Some(time);
        catalog.source_revision = Some(revision);
        let other = SessionRef::new(Provider::Claude, "neighbor");
        let batch = [
            (
                &other,
                physical_document("pearmarker", time, SourceRevision::default()),
                super::TrajectorySourceGuard::NewerSource,
                None,
            ),
            (
                &catalog.session,
                physical_document("plummarker", time, revision),
                store.observe_verified_session(&catalog.session).unwrap(),
                Some(&catalog),
            ),
        ];
        assert!(matches!(
            store.upsert_selected_trajectory_documents_checked(batch.into_iter(), |_, _| false),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(store.indexed_sessions().unwrap().is_empty());
        assert!(store.trajectory_index_states().unwrap().is_empty());

        let count = store
            .upsert_selected_trajectory_documents_checked(batch.into_iter(), |session, selected| {
                session == &catalog.session && selected == revision
            })
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            store.indexed_sessions().unwrap(),
            std::slice::from_ref(&catalog)
        );
        assert_eq!(
            store.search_session_trajectories("pearmarker", 1).unwrap(),
            [other]
        );
        assert_eq!(
            store.search_session_trajectories("plummarker", 1).unwrap(),
            [catalog.session]
        );
    }

    fn physical_document(
        text: &str,
        time: DateTime<Utc>,
        revision: SourceRevision,
    ) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            source_revision: revision,
            redacted_text: text,
            source_updated_at: time,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 6,
            derived_title: Some(text),
            title_authoritative: true,
        }
    }

    #[test]
    fn a_rejected_file_hides_stored_samples_until_a_full_read_succeeds() -> super::Result<()> {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3"))?;
        let time = DateTime::from_timestamp(1000, 843).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let metadata = IndexedSession {
            updated_at: Some(time),
            source_revision: Some(revision),
            ..indexed_session(Provider::Pi, "failed", "Synthetic header")
        };
        let session = &metadata.session;
        let observe = || store.observe_verified_session(session);
        let publish = |doc, guard| store.upsert_verified_session_document(&metadata, doc, guard);
        let sample = TrajectoryDocument {
            source_complete: false,
            ..physical_document("orphanedreply", time, revision)
        };
        publish(sample, observe()?)?;
        let before_failure = observe()?;
        let failure = super::TrajectoryIndexFailure {
            source_file: revision.file,
            source_contents: revision.contents,
            source_updated_at: Some(time),
            source_time_precise: true,
            document_version: sample.document_version,
        };
        store.record_observed_trajectory_index_failure(session, &failure, before_failure)?;
        assert!(
            store
                .search_session_trajectories("orphanedreply", 1)?
                .is_empty()
        );
        assert_eq!(store.trajectory_title(session)?, None);
        for guard in [before_failure, observe()?] {
            for provider_limited in [false, true] {
                assert!(matches!(
                    publish(
                        TrajectoryDocument {
                            provider_limited,
                            ..sample
                        },
                        guard
                    ),
                    Err(StoreError::TrajectoryPreviewRejected)
                ));
            }
        }
        assert_eq!(store.trajectory_index_failure(session)?, Some(failure));

        let verified = TrajectoryDocument {
            source_complete: true,
            source_byte_count: sample.indexed_byte_count + 10,
            truncation_strategy: "event_head_tail",
            ..sample
        };
        publish(verified, observe()?)?;
        assert_eq!(store.trajectory_index_failure(session)?, None);
        assert_eq!(
            store.search_session_trajectories("orphanedreply", 1)?,
            std::slice::from_ref(session)
        );
        let before_failure = observe()?;
        store.record_observed_trajectory_index_failure(session, &failure, before_failure)?;
        assert!(matches!(
            publish(verified, before_failure),
            Err(StoreError::TrajectorySourceChanged)
        ));
        publish(verified, observe()?)?;
        assert_eq!(store.trajectory_index_failure(session)?, None);
        Ok(())
    }

    #[test]
    fn physical_migration_preserves_legacy_workspace_and_full_search_until_paired_read() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp(1000, 843).unwrap();
        let mut old = indexed_session(Provider::OhMyPi, "moved", "Legacy");
        old.updated_at = Some(time);
        let session = &old.session;
        let hit = |query| {
            store
                .search_session_trajectories(query, 1)
                .unwrap()
                .contains(session)
        };
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let current = IndexedSession {
            source_revision: Some(revision),
            project_path: Some("/new".into()),
            ..old.clone()
        };
        let legacy = physical_document("archivedmiddle", time, SourceRevision::default());
        let updated = physical_document("freshsource", time, revision);
        let sample = TrajectoryDocument {
            source_complete: false,
            ..updated
        };
        store.upsert_indexed_session(&old).unwrap();
        store.upsert_trajectory_document(session, &legacy).unwrap();
        store
            .merge_indexed_sessions(session.provider, std::slice::from_ref(&current))
            .unwrap();
        store
            .replace_indexed_sessions(session.provider, std::slice::from_ref(&current))
            .unwrap();
        let guard = store.observe_trajectory_document(session).unwrap();
        let write = |doc| {
            store
                .upsert_observed_trajectory_documents_checked(&[(session, doc, guard)], |_, _| true)
                .unwrap()
        };
        assert_eq!(write(sample), 0);
        assert_eq!(write(updated), 0);
        let publish = |doc, guard| {
            store
                .upsert_verified_session_document_checked(&current, doc, guard, |_, selected| {
                    selected == revision
                })
                .unwrap()
        };
        let paired = store.observe_verified_session(session).unwrap();
        assert_eq!(publish(sample, paired), 0);
        assert!(hit("archivedmiddle"));
        assert_eq!(
            store.indexed_sessions().unwrap(),
            std::slice::from_ref(&old)
        );
        assert_eq!(publish(updated, paired), 1);
        assert_eq!(
            store.indexed_sessions().unwrap(),
            std::slice::from_ref(&current)
        );
        assert!(!hit("archivedmiddle"));
        let replaced = SourceRevision {
            contents: Some(Uuid::from_u128(3)),
            ..revision
        };
        store
            .upsert_indexed_session(&IndexedSession {
                source_revision: Some(replaced),
                ..current.clone()
            })
            .unwrap();
        assert!(!hit("freshsource"));
        assert_eq!(store.trajectory_title(session).unwrap(), None);
        let failure = super::TrajectoryIndexFailure {
            source_file: replaced.file,
            source_contents: replaced.contents,
            source_updated_at: Some(time),
            source_time_precise: true,
            document_version: 6,
        };
        store
            .record_observed_trajectory_index_failure_checked(
                session,
                &failure,
                store.observe_trajectory_document(session).unwrap(),
                |_, r| r == replaced,
            )
            .unwrap();
        store.upsert_indexed_session(&current).unwrap();
        assert!(!hit("freshsource"));
        publish(updated, store.observe_verified_session(session).unwrap());
        assert!(store.trajectory_index_failures().unwrap().is_empty());
        assert!(hit("freshsource"));
    }

    #[test]
    fn confirmed_file_failure_hides_superseded_catalog_metadata() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp(1000, 0).unwrap();
        let revision = |n| SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(n)),
            missing_ancestor: None,
        };
        let failed = revision(2);
        let failure = super::TrajectoryIndexFailure {
            source_file: failed.file,
            source_contents: failed.contents,
            source_updated_at: Some(time),
            source_time_precise: true,
            document_version: 6,
        };
        for (provider, previous) in [(Provider::Pi, None), (Provider::OhMyPi, Some(revision(1)))] {
            let old = IndexedSession {
                source_revision: previous,
                updated_at: Some(time),
                ..indexed_session(provider, "moved", "Old title")
            };
            let session = &old.session;
            store.upsert_indexed_session(&old).unwrap();
            let observed = store.observe_verified_session(session).unwrap();
            let record = |selected| {
                store.record_observed_trajectory_index_failure_checked(
                    session,
                    &failure,
                    observed,
                    |_, source| source == selected,
                )
            };
            let listed = || store.indexed_sessions_for_provider(provider).unwrap();
            assert!(matches!(
                record(revision(3)),
                Err(StoreError::TrajectorySourceChanged)
            ));
            assert_eq!(listed(), std::slice::from_ref(&old));
            record(failed).unwrap();
            assert!(listed().is_empty());
            assert!(
                store
                    .indexed_sessions()
                    .unwrap()
                    .iter()
                    .all(|entry| entry.session != *session)
            );
            let replacement = IndexedSession {
                source_revision: Some(failed),
                title: Some("Current title".into()),
                project_path: Some("/new".into()),
                ..old.clone()
            };
            store.upsert_indexed_session(&replacement).unwrap();
            assert_eq!(listed(), std::slice::from_ref(&replacement));
            let observed = store.observe_verified_session(session).unwrap();
            store
                .upsert_verified_session_document(
                    &replacement,
                    physical_document("currentbody", time, failed),
                    observed,
                )
                .unwrap();
            assert!(
                !store
                    .trajectory_index_failures()
                    .unwrap()
                    .contains_key(session)
            );
            assert!(matches!(
                record(failed),
                Err(StoreError::TrajectorySourceChanged)
            ));
            assert_eq!(listed(), std::slice::from_ref(&replacement));
        }
    }

    #[test]
    fn document_observation_rejects_stale_equal_time_writes_after_insert_or_replacement() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("store.sqlite3");
        let background = Store::open(&path).unwrap();
        let show = Store::open(&path).unwrap();
        let time = DateTime::from_timestamp(1_000, 843).unwrap();
        let document = |text: &'static str| TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at: time,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 5,
            derived_title: Some(text),
            title_authoritative: false,
        };
        let old = document("withdrawn");
        let restored = document("replacement");
        for (provider, existing) in [(Provider::Pi, false), (Provider::OhMyPi, true)] {
            let mut catalog = indexed_session(provider, "concurrent-index", "Native title");
            catalog.updated_at = Some(time);
            let session = &catalog.session;
            if existing {
                show.upsert_trajectory_document(session, &old).unwrap();
            }
            let delayed = background.observe_trajectory_document(session).unwrap();
            let before_show = show.observe_verified_session(session).unwrap();
            show.upsert_verified_session_document(&catalog, restored, before_show)
                .unwrap();
            assert!(matches!(
                background.upsert_observed_trajectory_documents_counted(&[(session, old, delayed)]),
                Err(StoreError::TrajectorySourceChanged)
            ));
            assert!(
                background
                    .search_session_trajectories("withdrawn", 1)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                background
                    .search_session_trajectories("replacement", 2)
                    .unwrap()
                    .contains(session)
            );

            let current = background.observe_trajectory_document(session).unwrap();
            let bounded = TrajectoryDocument {
                source_byte_count: 12_000,
                truncation_strategy: "event_head_tail",
                ..document("currenttail")
            };
            assert_eq!(
                background
                    .upsert_observed_trajectory_documents_counted(&[(session, bounded, current)])
                    .unwrap(),
                1
            );
            assert!(
                show.search_session_trajectories("currenttail", 2)
                    .unwrap()
                    .contains(session)
            );
        }
    }

    #[test]
    fn restored_file_wins_over_a_reader_that_started_before_the_restore() {
        use super::TrajectorySourceGuard::Observed;

        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Pi, "restored-session");
        let original = DateTime::from_timestamp(1_000, 500).unwrap();
        let restored = original - chrono::Duration::nanoseconds(1);
        let write = |text: &str, time, version, guard| {
            let document = TrajectoryDocument {
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at: time,
                source_byte_count: text.len(),
                indexed_byte_count: text.len(),
                truncation_strategy: "none",
                source_complete: true,
                provider_limited: false,
                origin: SessionTrajectoryOrigin::Native,
                document_version: version,
                derived_title: None,
                title_authoritative: false,
            };
            store.upsert_observed_trajectory_documents_counted(&[(&session, document, guard)])
        };
        assert_eq!(write("withdrawn", original, 4, Observed(None)).unwrap(), 1);
        let saw_original = Observed(Some(original));
        assert_eq!(write("replacement", restored, 5, saw_original).unwrap(), 1);
        assert!(matches!(
            write("withdrawn", original, 5, saw_original),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(
            write("withdrawn", original, 4, Observed(Some(restored))).unwrap(),
            0
        );
        let state = store.trajectory_index_state(&session).unwrap().unwrap();
        assert!(state.source_time_precise);
        assert_eq!(state.source_updated_at, restored);
        assert!(
            store
                .search_session_trajectories("withdrawn", 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search_session_trajectories("replacement", 1).unwrap(),
            [session]
        );
    }

    #[test]
    fn omp_title_upgrade_discards_only_an_older_unverified_sample() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("index.sqlite3")).unwrap();
        let time = Utc::now();
        for (id, provider, full, verified, upgrade, retain) in [
            ("guess", Provider::OhMyPi, false, false, true, false),
            ("same-format", Provider::OhMyPi, false, false, false, true),
            ("known-name", Provider::OhMyPi, false, true, true, true),
            ("full", Provider::OhMyPi, true, true, true, true),
            ("other-provider", Provider::Pi, false, false, true, true),
        ] {
            let session = SessionRef::new(provider, id);
            let original = TrajectoryDocument {
                source_revision: SourceRevision::default(),
                redacted_text: "row",
                source_updated_at: time,
                source_byte_count: 3,
                indexed_byte_count: 3,
                source_complete: full,
                provider_limited: false,
                truncation_strategy: "none",
                origin: SessionTrajectoryOrigin::Native,
                document_version: 4,
                derived_title: Some("Prior name"),
                title_authoritative: verified,
            };
            let refreshed = TrajectoryDocument {
                source_updated_at: time + chrono::Duration::seconds(1),
                source_complete: false,
                provider_limited: true,
                document_version: original.document_version + u32::from(upgrade),
                title_authoritative: false,
                derived_title: None,
                ..original
            };
            for (doc, prior) in [(original, None), (refreshed, Some(time))] {
                store
                    .upsert_observed_trajectory_documents(&[(
                        &session,
                        doc,
                        super::TrajectorySourceGuard::Observed(prior),
                    )])
                    .unwrap();
            }
            assert_eq!(
                store.trajectory_title(&session).unwrap().as_deref(),
                retain.then_some("Prior name"),
                "{id}"
            );
        }
    }

    #[test]
    fn observed_reads_keep_known_titles_until_the_full_source_can_clear_them() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::OhMyPi, "sampled-title");
        let first = Utc::now();
        let mut prior = None;
        let original = Some("Original request");
        let updated = Some("Updated request");
        for (offset, full_read, strategy, title, expected) in [
            (0, true, "none", original, original),
            (1, false, "none", None, original),
            (2, false, "none", updated, updated),
            (3, true, "source_incomplete", None, updated),
            (3, true, "document_head_tail", None, None),
            (3, true, "document_head_tail", updated, updated),
            (3, true, "document_head_tail", None, None),
        ] {
            let time = first + chrono::Duration::seconds(offset);
            let text = "Synthetic visible text";
            let document = TrajectoryDocument {
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at: time,
                source_byte_count: text.len() + usize::from(strategy == "document_head_tail"),
                indexed_byte_count: text.len(),
                source_complete: full_read,
                provider_limited: false,
                truncation_strategy: strategy,
                origin: SessionTrajectoryOrigin::Native,
                document_version: 4,
                derived_title: title,
                title_authoritative: false,
            };
            store
                .upsert_observed_trajectory_documents(&[(
                    &session,
                    document,
                    super::TrajectorySourceGuard::Observed(prior),
                )])
                .unwrap();
            assert_eq!(
                store.trajectory_title(&session).unwrap().as_deref(),
                expected,
                "time offset {offset}, strategy {strategy}"
            );
            prior = Some(time);
        }
    }

    #[test]
    fn preview_does_not_claim_current_source_state() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "preview");
        let source_updated_at = Utc::now();
        let preview = "bounded preview";
        store
            .upsert_session_trajectory_document(
                &session,
                preview,
                source_updated_at,
                preview.len(),
                preview.len(),
                "none",
                false,
                SessionTrajectoryOrigin::Native,
            )
            .expect("preview");

        assert!(
            !store
                .session_trajectory_source_is_current(&session, source_updated_at)
                .expect("source coverage")
        );
    }

    #[test]
    fn full_read_with_provider_omissions_upgrades_same_state_document() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "omissions");
        let source_updated_at = Utc::now();
        let text = "retained tool history";
        let document = |source_complete| TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: text.len() + 1,
            indexed_byte_count: text.len(),
            truncation_strategy: "source_incomplete_event_head_tail",
            source_complete,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 2,
            derived_title: None,
            title_authoritative: false,
        };
        // Earlier writers stored provider omissions as an incomplete read.
        store
            .upsert_trajectory_document(&session, &document(false))
            .expect("earlier write");
        assert!(!store.trajectory_index_states().expect("states")[&session].source_complete);

        store
            .upsert_trajectory_document(&session, &document(true))
            .expect("full read");

        assert!(store.trajectory_index_states().expect("states")[&session].source_complete);
        assert!(
            store
                .session_trajectory_source_is_current(&session, source_updated_at)
                .expect("source coverage")
        );
        let matches = store
            .search_session_trajectory_matches("retained", 10)
            .expect("search full read");
        assert!(matches[0].source_complete);
        assert!(!matches[0].complete);
    }

    #[test]
    fn batch_keeps_only_the_last_document_of_a_repeated_session() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "batch-repeated");
        let other = SessionRef::new(Provider::Codex, "batch-other");
        let source_updated_at = Utc::now();
        let document = |text| TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: str::len(text),
            indexed_byte_count: str::len(text),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_authoritative: false,
        };
        // The last document spans more chunks than one insert statement holds.
        let mut last = "synthetic filler text. ".repeat(
            (super::TRAJECTORY_CHUNK_INSERT_ROWS + 2) * super::TRAJECTORY_CHUNK_BYTE_LIMIT / 23,
        );
        last.push_str("lastdocumentmarker");

        store
            .upsert_trajectory_documents(&[
                (&session, document("firstdocumentmarker")),
                (&other, document("otherdocumentmarker")),
                (&session, document(&last)),
            ])
            .expect("write batch with a repeated session");

        let search = |query| {
            store
                .search_session_trajectories(query, 10)
                .expect("search batch")
        };
        assert!(search("firstdocumentmarker").is_empty());
        assert_eq!(search("lastdocumentmarker"), vec![session.clone()]);
        assert_eq!(search("otherdocumentmarker"), vec![other]);
        assert_eq!(search("synthetic filler"), vec![session]);
    }

    #[test]
    fn trajectory_document_batch_preserves_metadata_and_search_content() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let native = SessionRef::new(Provider::Codex, "batch-native");
        let imported = SessionRef::new(Provider::Imported, "batch-imported");
        let source_updated_at = Utc::now();
        let native_text = "native batch head-tail marker";
        let imported_text = "imported batch marker";
        let native_document = TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: native_text,
            source_updated_at,
            source_byte_count: native_text.len() + 256,
            indexed_byte_count: native_text.len(),
            truncation_strategy: "document_head_tail",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 7,
            derived_title: Some("Synthetic native title"),
            title_authoritative: false,
        };
        let imported_document = TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: imported_text,
            source_updated_at,
            source_byte_count: imported_text.len(),
            indexed_byte_count: imported_text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::ImportedBundle,
            document_version: 8,
            derived_title: Some("Synthetic imported title"),
            title_authoritative: false,
        };

        store
            .upsert_trajectory_documents(&[
                (&native, native_document),
                (&imported, imported_document),
            ])
            .expect("write bounded batch");

        let states = store.trajectory_index_states().expect("index states");
        assert_eq!(states[&native].document_version, 7);
        assert!(states[&native].source_complete);
        assert_eq!(states[&imported].document_version, 8);
        assert!(states[&imported].source_complete);
        assert_eq!(
            store.trajectory_titles().expect("trajectory titles")[&native],
            "Synthetic native title"
        );
        assert_eq!(
            store
                .search_session_trajectories("native batch", 10)
                .expect("search native batch"),
            vec![native.clone()]
        );
        assert_eq!(
            store
                .search_session_trajectories("imported batch", 10)
                .expect("search imported batch"),
            vec![imported]
        );
        assert!(
            store
                .session_trajectory_source_is_current(&native, source_updated_at)
                .expect("native source coverage")
        );
    }

    #[test]
    fn trajectory_document_batch_rolls_back_all_rows_on_database_failure() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TRIGGER reject_synthetic_batch_document
                 BEFORE INSERT ON session_trajectory_chunk_rows
                 WHEN instr(omnis_chunk_text(NEW.stored_text), 'reject-batch-marker') > 0
                 BEGIN
                     SELECT RAISE(ABORT, 'synthetic batch write failure');
                 END;",
            )
            .expect("install synthetic failure trigger");

        let first = SessionRef::new(Provider::Codex, "batch-first");
        let second = SessionRef::new(Provider::Claude, "batch-second");
        let source_updated_at = Utc::now();
        let first_text = "first batch content marker";
        let second_text = "second reject-batch-marker content";
        let document = |text| TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_authoritative: false,
        };

        assert!(
            store
                .upsert_trajectory_documents(&[
                    (&first, document(first_text)),
                    (&second, document(second_text)),
                ])
                .is_err()
        );
        assert!(store.trajectory_index_states().expect("states").is_empty());
        assert!(
            store
                .search_session_trajectories("first batch content", 10)
                .expect("search rolled back content")
                .is_empty()
        );
    }

    #[test]
    fn trajectory_document_batch_enforces_bounds_without_limiting_single_writes() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "bounded-batch");
        let text = format!(
            "{}tail marker",
            "synthetic search text ".repeat(
                super::MAX_TRAJECTORY_WRITE_BATCH_BYTES / "synthetic search text ".len() + 1
            )
        );
        let document = TrajectoryDocument {
            source_revision: SourceRevision::default(),
            redacted_text: &text,
            source_updated_at: Utc::now(),
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_authoritative: false,
        };

        assert!(
            store
                .upsert_trajectory_documents(&[(&session, document)])
                .is_err()
        );
        store
            .upsert_trajectory_document(&session, &document)
            .expect("single-document write remains unbounded");
        assert_eq!(
            store
                .search_session_trajectories("tail marker", 10)
                .expect("search single large document"),
            vec![session]
        );
    }

    #[test]
    fn trajectory_coverage_rejects_impossible_byte_counts_and_strategies() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "invalid-coverage");
        let text = "bounded text";
        let upsert = |source_byte_count, strategy| {
            store.upsert_session_trajectory_document(
                &session,
                text,
                Utc::now(),
                source_byte_count,
                text.len(),
                strategy,
                true,
                SessionTrajectoryOrigin::Native,
            )
        };

        assert!(upsert(text.len() - 1, "legacy_unknown").is_err());
        assert!(upsert(text.len() + 1, "none").is_err());
        assert!(upsert(text.len() + 1, "source_incomplete").is_err());
        assert!(upsert(text.len(), "document_head_tail").is_err());
        assert!(upsert(text.len() + 1, "document_head_tail").is_ok());
    }

    #[test]
    fn overlapping_chunks_keep_boundary_phrase_searchable_and_bounded() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "chunk-boundary");
        let phrase = (0..TRAJECTORY_PHRASE_MAX_TOKENS)
            .map(|index| format!("boundaryterm{index:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(phrase.len() > 512);
        let mut text = "x".repeat(TRAJECTORY_CHUNK_BYTE_LIMIT - 700);
        text.push(' ');
        text.push_str(&phrase);
        store
            .upsert_session_trajectory(&session, &text, Utc::now(), true)
            .expect("chunk trajectory");

        let query = format!("\"{phrase}\"");
        let matches = store
            .search_session_trajectory_matches(&query, 10)
            .expect("long boundary phrase");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session, session);
        assert_eq!(matches[0].indexed_byte_count, text.len());
        let connection = store.connection.borrow();
        let maximum_chunk_bytes = connection
            .query_row(
                "SELECT max(length(CAST(redacted_text AS BLOB)))
                 FROM session_trajectory_chunks",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("chunk size");
        assert!(maximum_chunk_bytes <= i64::try_from(TRAJECTORY_CHUNK_BYTE_LIMIT).unwrap());
    }

    #[test]
    fn every_text_of_maximum_phrase_length_lies_whole_in_one_chunk() {
        // One-byte text, four-byte text, and a mix with paragraph breaks, which move chunk ends.
        let ascii = "synthetic words. ".repeat(20_000);
        let four_byte = "\u{1f600}".repeat(90_000);
        let mixed = "ab \u{e9} \u{6f22} \u{1f600} word\n\nnext line ".repeat(12_000);
        for (text, shared_bytes) in [
            (&ascii, Some(SEARCH_QUERY_MAX_CHARS)),
            (&four_byte, Some(TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES)),
            (&mixed, None),
        ] {
            let chunks = utf8_chunks(text, TRAJECTORY_CHUNK_BYTE_LIMIT)
                .map(|chunk| {
                    let start = chunk.as_ptr() as usize - text.as_ptr() as usize;
                    start..start + chunk.len()
                })
                .collect::<Vec<_>>();
            assert!(chunks.len() > 2);
            assert_eq!(chunks.last().map(|chunk| chunk.end), Some(text.len()));
            for pair in chunks.windows(2) {
                assert!(pair[0].len() <= TRAJECTORY_CHUNK_BYTE_LIMIT);
                let shared = &text[pair[1].start..pair[0].end];
                assert_eq!(shared.chars().count(), SEARCH_QUERY_MAX_CHARS);
                if let Some(shared_bytes) = shared_bytes {
                    assert_eq!(shared.len(), shared_bytes);
                }
            }

            let boundaries = text
                .char_indices()
                .map(|(index, _)| index)
                .chain([text.len()])
                .collect::<Vec<_>>();
            let mut chunk = 0;
            for window in boundaries.windows(SEARCH_QUERY_MAX_CHARS + 1) {
                let (start, end) = (window[0], window[SEARCH_QUERY_MAX_CHARS]);
                // Text that starts later lies in the same chunk or in a later one.
                while end > chunks[chunk].end {
                    chunk += 1;
                }
                assert!(
                    chunks[chunk].start <= start,
                    "characters at bytes {start}..{end} are split across chunks"
                );
            }
        }
    }

    #[test]
    fn full_text_index_merges_segments_less_often_than_the_default() {
        let temporary_directory = tempdir().expect("temporary directory");
        let path = temporary_directory.path().join("store.sqlite3");
        // The second open finds the setting and leaves it alone.
        for _ in 0..2 {
            let store = Store::open(&path).expect("store");
            let automerge = store
                .connection
                .borrow()
                .query_row(
                    "SELECT v FROM session_trajectory_chunks_fts_config WHERE k = 'automerge'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("stored automerge setting");
            assert_eq!(automerge, super::FTS_AUTOMERGE_SEGMENTS);
        }
    }

    /// Text that compresses about as well as a real session: varied words with repeated terms.
    fn compressible_text(bytes: usize) -> String {
        let mut text = String::with_capacity(bytes + 64);
        let mut index = 0_u32;
        while text.len() < bytes {
            writeln!(
                text,
                "step {index} runs cargo test for module{} and reports result {}.",
                index % 97,
                index.wrapping_mul(2_654_435_761) % 10_007
            )
            .expect("write to a string");
            index += 1;
        }
        text
    }

    #[test]
    fn chunk_text_is_stored_compressed_and_reads_back_unchanged() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let large = SessionRef::new(Provider::Codex, "compressed");
        let small = SessionRef::new(Provider::Codex, "plain");
        let mut text = compressible_text(3 * TRAJECTORY_CHUNK_BYTE_LIMIT);
        text.push_str("uniquecompressedmarker closes the document");
        store
            .upsert_session_trajectory(&large, &text, Utc::now(), true)
            .expect("large trajectory");
        store
            .upsert_session_trajectory(&small, "tiny note", Utc::now(), true)
            .expect("small trajectory");

        let connection = store.connection.borrow();
        let stored = |session: &SessionRef| {
            connection
                .query_row(
                    "SELECT group_concat(DISTINCT typeof(rows.stored_text)),
                            sum(length(rows.stored_text))
                     FROM session_trajectory_chunk_rows AS rows
                     INNER JOIN session_trajectories AS trajectories
                         ON trajectories.id = rows.trajectory_id
                     WHERE trajectories.session_id = ?1",
                    [&session.id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("stored chunk form")
        };
        let (large_type, large_bytes) = stored(&large);
        assert_eq!(large_type, "blob");
        assert!(
            usize::try_from(large_bytes).expect("stored size") < text.len() / 2,
            "{large_bytes} of {}",
            text.len()
        );
        // LZ4 does not make a tiny chunk smaller, so it stays plain text.
        assert_eq!(stored(&small), ("text".to_owned(), 9));

        let read_back = connection
            .prepare(
                "SELECT chunks.redacted_text FROM session_trajectory_chunks AS chunks
                 INNER JOIN session_trajectories AS trajectories
                     ON trajectories.id = chunks.trajectory_id
                 WHERE trajectories.session_id = ?1
                 ORDER BY chunks.chunk_index",
            )
            .expect("chunk view")
            .query_map([&large.id], |row| row.get::<_, String>(0))
            .expect("read chunks")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("chunk text");
        assert_eq!(
            read_back,
            utf8_chunks(&text, TRAJECTORY_CHUNK_BYTE_LIMIT).collect::<Vec<_>>()
        );
        drop(connection);

        for query in [
            "uniquecompressedmarker",
            "\"uniquecompressedmarker closes\"",
        ] {
            let matches = store
                .search_session_trajectory_matches(query, 10)
                .expect("search compressed text");
            assert_eq!(matches.len(), 1, "{query}");
            assert_eq!(matches[0].session, large);
            assert!(
                matches[0].snippet.contains("uniquecompressedmarker"),
                "{query}: {}",
                matches[0].snippet
            );
        }
    }

    #[test]
    fn store_of_a_newer_format_is_not_opened() {
        let temporary_directory = tempdir().expect("temporary directory");
        let path = temporary_directory.path().join("store.sqlite3");
        let format = |store: &Store| {
            store
                .connection
                .borrow()
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("store format")
        };
        let store = Store::open(&path).expect("store");
        assert_eq!(format(&store), super::STORE_FORMAT);
        store
            .connection
            .borrow()
            .pragma_update(None, "user_version", super::STORE_FORMAT + 1)
            .expect("mark a newer format");
        drop(store);

        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerStoreFormat)
        ));
    }

    #[test]
    fn chunk_text_function_runs_when_the_schema_is_not_trusted() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "untrusted-schema");
        store
            .connection
            .borrow()
            .pragma_update(None, "trusted_schema", false)
            .expect("stop trusting the schema");

        // The insert trigger, the view behind the snippet, and the delete trigger call the
        // function from the schema.
        store
            .upsert_session_trajectory(&session, &compressible_text(8_192), Utc::now(), true)
            .expect("write with an untrusted schema");
        let matches = store
            .search_session_trajectory_matches("module42", 10)
            .expect("search with an untrusted schema");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].snippet.contains("module42"));
        store
            .upsert_session_trajectory(
                &session,
                "replaced",
                Utc::now() + chrono::Duration::seconds(1),
                true,
            )
            .expect("replace with an untrusted schema");
    }

    /// Changes the stored bytes of every compressed chunk without the update triggers, like
    /// damage to the file would.
    fn damage_compressed_chunks(store: &Store) {
        store
            .connection
            .borrow()
            .execute_batch(
                "DROP TRIGGER session_trajectory_chunk_rows_after_update;
                 DROP TRIGGER session_trajectory_chunk_rows_unreadable_update;
                 UPDATE session_trajectory_chunk_rows SET stored_text = X'0200000000'
                 WHERE typeof(stored_text) = 'blob';",
            )
            .expect("damage the compressed chunks");
    }

    fn assert_full_text_index_matches_chunks(store: &Store) {
        store
            .connection
            .borrow()
            .execute(
                "INSERT INTO session_trajectory_chunks_fts (session_trajectory_chunks_fts, rank)
                 VALUES ('integrity-check', 1)",
                [],
            )
            .expect("full-text index matches the stored chunks");
    }

    #[test]
    fn damaged_chunk_does_not_block_search_or_a_new_index_pass() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let intact = SessionRef::new(Provider::Codex, "intact");
        let damaged = SessionRef::new(Provider::Codex, "damaged");
        let source_updated_at = Utc::now();
        store
            .upsert_session_trajectory(&intact, "intactmarker text", source_updated_at, true)
            .expect("intact trajectory");
        // The damaged chunk is the newest row, so the next chunk takes its ID once it is gone.
        store
            .upsert_session_trajectory(
                &damaged,
                &format!("{} damagedmarker", compressible_text(8_192)),
                source_updated_at,
                true,
            )
            .expect("trajectory to damage");
        damage_compressed_chunks(&store);

        let search = |query| {
            store
                .search_session_trajectory_matches(query, 10)
                .expect("search a store with a damaged chunk")
        };
        // The index still holds the tokens of the damaged chunk. Its text reads as empty.
        let matches = search("damagedmarker");
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].snippet.contains("damagedmarker"));
        assert!(search("\"damagedmarker\"").is_empty());
        assert_eq!(search("intactmarker").len(), 1);

        store
            .upsert_session_trajectory(
                &damaged,
                "repairedmarker text",
                source_updated_at + chrono::Duration::seconds(1),
                true,
            )
            .expect("index the damaged session again");
        assert!(search("damagedmarker").is_empty());
        assert!(search("module42").is_empty());
        assert_eq!(search("repairedmarker").len(), 1);
        assert_eq!(search("intactmarker").len(), 1);
        assert_full_text_index_matches_chunks(&store);
    }

    #[test]
    fn chunk_after_a_forgotten_damaged_session_does_not_take_its_tokens() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let damaged = SessionRef::new(Provider::Codex, "damaged");
        let later = SessionRef::new(Provider::Codex, "later");
        store
            .upsert_session_trajectory(
                &damaged,
                &format!("{} damagedmarker", compressible_text(8_192)),
                Utc::now(),
                true,
            )
            .expect("trajectory to damage");
        damage_compressed_chunks(&store);
        store.forget_session(&damaged).expect("forget the session");

        // This chunk takes the row ID of the damaged chunk.
        store
            .upsert_session_trajectory(&later, "latermarker text", Utc::now(), true)
            .expect("later trajectory");
        let search = |query| {
            store
                .search_session_trajectories(query, 10)
                .expect("search after the damaged session is gone")
        };
        assert!(search("damagedmarker").is_empty());
        assert!(search("module42").is_empty());
        assert_eq!(search("latermarker"), vec![later]);
        assert_full_text_index_matches_chunks(&store);
    }

    #[test]
    fn stored_chunks_that_this_version_cannot_read_are_rejected() {
        let text = compressible_text(4_096);
        let StoredChunk::Lz4(stored) = encode_chunk(&text) else {
            panic!("compressible text must compress");
        };
        let mut scratch = Vec::new();
        assert_eq!(
            chunk_text(ValueRef::Blob(&stored), &mut scratch),
            Some(text.as_str())
        );
        assert_eq!(
            chunk_text(ValueRef::Text(text.as_bytes()), &mut scratch),
            Some(text.as_str())
        );
        assert!(matches!(encode_chunk(""), StoredChunk::Plain("")));

        let mut unknown_codec = stored.clone();
        unknown_codec[0] = 2;
        let mut longer_than_a_chunk = stored.clone();
        longer_than_a_chunk[1..CHUNK_HEADER_BYTES].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut wrong_length = stored.clone();
        wrong_length[1..CHUNK_HEADER_BYTES]
            .copy_from_slice(&u32::try_from(text.len() + 1).expect("length").to_le_bytes());
        let truncated = &stored[..stored.len() - 7];
        let invalid_utf8 = match encode_chunk(&"a".repeat(64)) {
            StoredChunk::Lz4(mut stored) => {
                // The block starts with a token byte and then the first literal.
                stored[CHUNK_HEADER_BYTES + 1] = 0xff;
                stored
            }
            StoredChunk::Plain(_) => panic!("repeated text must compress"),
        };
        for unreadable in [
            unknown_codec.as_slice(),
            longer_than_a_chunk.as_slice(),
            wrong_length.as_slice(),
            truncated,
            invalid_utf8.as_slice(),
            &stored[..3],
            &[],
        ] {
            assert_eq!(chunk_text(ValueRef::Blob(unreadable), &mut scratch), None);
        }
        assert_eq!(chunk_text(ValueRef::Integer(1), &mut scratch), None);
    }

    /// Puts the chunk table back to the layout that versions before compression wrote: one table
    /// with plain text, and triggers that pass the text to the index.
    fn use_plain_chunk_layout(store: &Store) {
        store
            .connection
            .borrow()
            .execute_batch(
                "DROP VIEW session_trajectory_chunks;
             DROP TRIGGER session_trajectory_chunk_rows_after_insert;
             DROP TRIGGER session_trajectory_chunk_rows_after_delete;
             DROP TRIGGER session_trajectory_chunk_rows_after_update;
             UPDATE session_trajectory_chunk_rows
                 SET stored_text = omnis_chunk_text(stored_text);
             ALTER TABLE session_trajectory_chunk_rows
                 RENAME COLUMN stored_text TO redacted_text;
             ALTER TABLE session_trajectory_chunk_rows
                 RENAME TO session_trajectory_chunks;
             CREATE TRIGGER session_trajectory_chunks_after_insert
             AFTER INSERT ON session_trajectory_chunks BEGIN
                 INSERT INTO session_trajectory_chunks_fts (rowid, redacted_text)
                 VALUES (new.id, new.redacted_text);
             END;
             CREATE TRIGGER session_trajectory_chunks_after_delete
             AFTER DELETE ON session_trajectory_chunks BEGIN
                 INSERT INTO session_trajectory_chunks_fts (
                     session_trajectory_chunks_fts, rowid, redacted_text
                 ) VALUES ('delete', old.id, old.redacted_text);
             END;
             CREATE TRIGGER session_trajectory_chunks_after_update
             AFTER UPDATE ON session_trajectory_chunks BEGIN
                 INSERT INTO session_trajectory_chunks_fts (
                     session_trajectory_chunks_fts, rowid, redacted_text
                 ) VALUES ('delete', old.id, old.redacted_text);
                 INSERT INTO session_trajectory_chunks_fts (rowid, redacted_text)
                 VALUES (new.id, new.redacted_text);
             END;",
            )
            .expect("earlier chunk layout");
    }

    #[test]
    fn store_with_plain_chunks_from_an_earlier_version_keeps_working() {
        let temporary_directory = tempdir().expect("temporary directory");
        let path = temporary_directory.path().join("store.sqlite3");
        let earlier = SessionRef::new(Provider::Codex, "earlier");
        let mut earlier_text = compressible_text(2 * TRAJECTORY_CHUNK_BYTE_LIMIT);
        earlier_text.push_str("earlierversionmarker ends the plain document");
        let source_updated_at = Utc::now();
        {
            let store = Store::open(&path).expect("store");
            store
                .upsert_session_trajectory(&earlier, &earlier_text, source_updated_at, true)
                .expect("earlier trajectory");
            use_plain_chunk_layout(&store);
        }

        // The second open must find the new layout and change nothing.
        for _ in 0..2 {
            let store = Store::open(&path).expect("open earlier store");
            let connection = store.connection.borrow();
            let plain_chunks = connection
                .query_row(
                    "SELECT count(*) FROM session_trajectory_chunk_rows
                     WHERE typeof(stored_text) = 'text'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("plain chunk count");
            assert!(plain_chunks > 2);
            drop(connection);
            for query in ["earlierversionmarker", "\"earlierversionmarker ends\""] {
                let matches = store
                    .search_session_trajectory_matches(query, 10)
                    .expect("search plain chunks");
                assert_eq!(matches.len(), 1, "{query}");
                assert!(
                    matches[0].snippet.contains("earlierversionmarker"),
                    "{query}"
                );
            }
        }

        let store = Store::open(&path).expect("open earlier store");
        let later = SessionRef::new(Provider::Codex, "later");
        store
            .upsert_session_trajectory(&later, &compressible_text(8_192), Utc::now(), true)
            .expect("later trajectory");
        // Indexing the earlier session again removes its plain chunks from the index.
        store
            .upsert_session_trajectory(
                &earlier,
                "replacementmarker only",
                source_updated_at + chrono::Duration::seconds(1),
                true,
            )
            .expect("replace earlier trajectory");
        let search = |query| {
            store
                .search_session_trajectories(query, 10)
                .expect("search migrated store")
        };
        assert!(search("earlierversionmarker").is_empty());
        assert_eq!(search("replacementmarker"), vec![earlier]);
        assert_eq!(search("module42"), vec![later]);
        store
            .connection
            .borrow()
            .execute(
                "INSERT INTO session_trajectory_chunks_fts (session_trajectory_chunks_fts, rank)
                 VALUES ('integrity-check', 1)",
                [],
            )
            .expect("full-text index matches the stored chunks");
    }

    #[test]
    fn cross_chunk_clauses_match_one_session_in_global_and_eligible_search() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "cross-chunk");
        let text = format!(
            "alpha first quoted {} second quoted omega",
            "x".repeat(TRAJECTORY_CHUNK_BYTE_LIMIT + TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES)
        );
        store
            .upsert_session_trajectory(&session, &text, Utc::now(), true)
            .expect("cross-chunk trajectory");
        let partial = SessionRef::new(Provider::Claude, "partial");
        store
            .upsert_session_trajectory(&partial, "alpha first quoted only", Utc::now(), true)
            .expect("partial trajectory");

        assert_eq!(
            store
                .search_session_trajectories("alpha omega", 10)
                .expect("cross-chunk terms"),
            vec![session.clone()]
        );
        let eligible = store
            .search_session_trajectory_page_for_sessions(
                "\"first quoted\" \"second quoted\"",
                10,
                &[session.clone(), partial],
            )
            .expect("cross-chunk phrases");
        assert_eq!(
            eligible
                .matches
                .into_iter()
                .map(|item| item.session)
                .collect::<Vec<_>>(),
            vec![session]
        );
    }

    #[test]
    fn identical_upsert_preserves_chunk_rows_and_index_timestamp() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "idempotent");
        let source_updated_at = Utc::now();
        let text = "unchanged indexed trajectory";
        store
            .upsert_session_trajectory(&session, text, source_updated_at, true)
            .expect("initial trajectory");
        let before = {
            let connection = store.connection.borrow();
            connection
                .query_row(
                    "SELECT chunks.id, trajectories.indexed_at
                     FROM session_trajectory_chunks AS chunks
                     INNER JOIN session_trajectories AS trajectories
                         ON trajectories.id = chunks.trajectory_id
                     WHERE trajectories.provider = ?1 AND trajectories.session_id = ?2",
                    params![session.provider.to_string(), session.id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("initial row")
        };

        store
            .upsert_session_trajectory(&session, text, source_updated_at, true)
            .expect("identical trajectory");

        let connection = store.connection.borrow();
        let after = connection
            .query_row(
                "SELECT chunks.id, trajectories.indexed_at
                 FROM session_trajectory_chunks AS chunks
                 INNER JOIN session_trajectories AS trajectories
                     ON trajectories.id = chunks.trajectory_id
                 WHERE trajectories.provider = ?1 AND trajectories.session_id = ?2",
                params![session.provider.to_string(), session.id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("unchanged row");
        assert_eq!(after, before);
    }

    #[test]
    fn limited_multi_clause_page_keeps_the_exact_snippet_and_eligible_order() {
        let directory = tempdir().expect("temporary directory");
        let store = Store::open(directory.path().join("store.sqlite3")).expect("store");
        let updated_at = Utc::now();
        let text = format!(
            "lattice first {} Exact Quartz.Needle remains visible",
            "x".repeat(TRAJECTORY_CHUNK_BYTE_LIMIT + TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES)
        );
        let sessions = ["a", "b", "c"].map(|id| SessionRef::new(Provider::Codex, id));
        for session in &sessions {
            store
                .upsert_session_trajectory(session, &text, updated_at, true)
                .expect("cross-chunk trajectory");
        }

        let query = "lattice \"quartz.needle\"";
        let page = store
            .search_session_trajectory_page(query, 1)
            .expect("bounded mixed-clause page");
        assert!(page.has_more);
        assert_eq!(page.matches.len(), 1);
        assert_eq!(page.matches[0].session, sessions[0]);
        assert!(page.matches[0].snippet.contains("Quartz.Needle"));

        let scoped = store
            .search_session_trajectory_page_for_sessions(query, 1, &sessions[1..])
            .expect("eligible mixed-clause page");
        assert!(scoped.has_more);
        assert_eq!(scoped.matches.len(), 1);
        assert_eq!(scoped.matches[0].session, sessions[1]);
        assert!(scoped.matches[0].snippet.contains("Quartz.Needle"));
    }

    #[test]
    fn trajectory_search_page_is_bounded_and_reports_more() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let source_updated_at = Utc::now();
        for id in ["c", "a", "b"] {
            store
                .upsert_session_trajectory(
                    &SessionRef::new(Provider::Codex, id),
                    "stable ranking marker",
                    source_updated_at,
                    true,
                )
                .expect("trajectory");
        }

        let page = store
            .search_session_trajectory_page("stable ranking", 2)
            .expect("bounded page");
        assert_eq!(page.matches.len(), 2);
        assert!(page.has_more);
        assert_eq!(
            page.matches
                .iter()
                .map(|item| item.session.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn eligible_search_ranks_and_limits_after_scope_and_provider_filtering() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let source_updated_at = Utc::now();
        for index in 0..=TRAJECTORY_SEARCH_RESULT_LIMIT {
            store
                .upsert_session_trajectory(
                    &SessionRef::new(Provider::Claude, format!("out-of-scope-{index:03}")),
                    "eligible needle",
                    source_updated_at,
                    true,
                )
                .expect("out-of-scope trajectory");
        }
        let in_scope = SessionRef::new(Provider::Codex, "in-scope");
        let weaker_text = format!("{} eligible needle", "synthetic filler ".repeat(100));
        store
            .upsert_session_trajectory(&in_scope, &weaker_text, source_updated_at, true)
            .expect("in-scope trajectory");

        let global = store
            .search_session_trajectory_page("eligible needle", 256)
            .expect("global ranked page");
        assert!(global.has_more);
        assert!(global.matches.iter().all(|item| item.session != in_scope));

        let eligible = store
            .search_session_trajectory_page_for_sessions(
                "eligible needle",
                256,
                std::slice::from_ref(&in_scope),
            )
            .expect("eligible ranked page");
        assert_eq!(eligible.matches.len(), 1);
        assert_eq!(eligible.matches[0].session, in_scope);
        assert!(!eligible.has_more);
    }

    #[test]
    fn failed_session_index_refresh_keeps_previous_snapshot() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let previous = indexed_session(Provider::Claude, "previous", "Previous title");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&previous))
            .expect("initial index");
        store
            .upsert_session_trajectory(
                &previous.session,
                "previous searchable trajectory",
                Utc::now(),
                true,
            )
            .expect("initial trajectory");
        let duplicate = indexed_session(Provider::Claude, "duplicate", "Duplicate title");

        assert!(
            store
                .replace_indexed_sessions(Provider::Claude, &[duplicate.clone(), duplicate],)
                .is_err()
        );
        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TRIGGER reject_partial_row BEFORE INSERT ON session_index
             WHEN NEW.session_id = 'rejected'
             BEGIN SELECT RAISE(ABORT, 'synthetic merge failure'); END;",
            )
            .expect("synthetic merge failure trigger");
        let mut changed = previous.clone();
        changed.title = Some("Changed title".to_owned());
        assert!(
            store
                .merge_indexed_sessions(
                    Provider::Claude,
                    &[
                        changed,
                        indexed_session(Provider::Claude, "rejected", "Rejected")
                    ],
                )
                .is_err()
        );
        assert_eq!(
            store.indexed_sessions().expect("read index"),
            vec![previous]
        );
        assert_eq!(
            store
                .search_session_trajectories("previous searchable", 10)
                .expect("preserved trajectory"),
            vec![SessionRef::new(Provider::Claude, "previous")]
        );
    }

    #[test]
    fn successful_refresh_prunes_only_stale_native_trajectories() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let native = indexed_session(Provider::Claude, "native", "Native");
        let imported = indexed_session(Provider::Claude, "imported", "Imported");
        store
            .replace_indexed_sessions(Provider::Claude, &[native.clone(), imported.clone()])
            .expect("initial refresh");
        store
            .upsert_session_trajectory(&native.session, "stale native marker", Utc::now(), true)
            .expect("native trajectory");
        let imported_text = "durable imported marker";
        store
            .upsert_session_trajectory_document(
                &imported.session,
                imported_text,
                Utc::now(),
                imported_text.len(),
                imported_text.len(),
                "none",
                true,
                SessionTrajectoryOrigin::ImportedBundle,
            )
            .expect("imported trajectory");

        store
            .replace_indexed_sessions(Provider::Claude, &[])
            .expect("successful empty refresh");

        assert!(
            store
                .search_session_trajectories("stale native", 10)
                .expect("native pruned")
                .is_empty()
        );
        assert_eq!(
            store
                .search_session_trajectories("durable imported", 10)
                .expect("import preserved"),
            vec![imported.session]
        );
    }

    #[test]
    fn bundle_protection_survives_newer_native_content_and_empty_refresh() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let indexed = indexed_session(Provider::Claude, "shared", "Shared");
        store
            .replace_indexed_sessions(Provider::Claude, std::slice::from_ref(&indexed))
            .expect("initial refresh");
        let source_updated_at = Utc::now();
        store
            .upsert_session_trajectory(
                &indexed.session,
                "newer native ownership marker",
                source_updated_at,
                true,
            )
            .expect("native trajectory");
        let older_import = "older imported marker";
        store
            .upsert_session_trajectory_document(
                &indexed.session,
                older_import,
                source_updated_at - chrono::Duration::seconds(1),
                older_import.len(),
                older_import.len(),
                "none",
                true,
                SessionTrajectoryOrigin::ImportedBundle,
            )
            .expect("import protection");

        assert_eq!(
            store
                .search_session_trajectories("newer native ownership", 10)
                .expect("older import did not replace"),
            vec![indexed.session.clone()]
        );
        assert!(
            store
                .search_session_trajectories("older imported", 10)
                .expect("older content excluded")
                .is_empty()
        );

        store
            .upsert_session_trajectory(
                &indexed.session,
                "newest native replacement marker",
                source_updated_at + chrono::Duration::seconds(1),
                true,
            )
            .expect("newer native replacement");

        store
            .replace_indexed_sessions(Provider::Claude, &[])
            .expect("successful empty refresh");

        assert_eq!(
            store
                .search_session_trajectories("newest native replacement", 10)
                .expect("protected replacement preserved"),
            vec![indexed.session]
        );
        assert!(
            store
                .search_session_trajectories("newer native ownership", 10)
                .expect("older native content replaced")
                .is_empty()
        );
    }

    #[test]
    fn session_index_tracks_empty_provider_refreshes() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");

        assert_eq!(
            store
                .session_index_refreshed_at(Provider::Grok)
                .expect("initial refresh state"),
            None
        );
        store
            .replace_indexed_sessions(Provider::Grok, &[])
            .expect("empty refresh");

        assert!(
            store
                .session_index_refreshed_at(Provider::Grok)
                .expect("refresh state")
                .is_some()
        );
        assert!(
            store
                .indexed_sessions_for_provider(Provider::Grok)
                .expect("empty index")
                .is_empty()
        );
        assert!(
            store
                .session_index_checked_at(Provider::Grok)
                .expect("check state")
                .is_some()
        );
    }

    #[test]
    fn existing_session_index_gains_approximate_timestamp_column() {
        let temporary_directory = tempdir().expect("temporary directory");
        let database = temporary_directory.path().join("store.sqlite3");
        let connection = rusqlite::Connection::open(&database).expect("old store");
        connection
            .execute_batch(
                "CREATE TABLE session_index (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    title TEXT,
                    project_path TEXT,
                    git_branch TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    event_count INTEGER NOT NULL,
                    indexed_at INTEGER NOT NULL,
                    PRIMARY KEY (provider, session_id)
                ) WITHOUT ROWID;",
            )
            .expect("old schema");
        drop(connection);

        let store = Store::open(&database).expect("upgraded store");
        let mut session = indexed_session(Provider::CursorCli, "approximate", "Approximate");
        session.updated_at = chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis());
        session.updated_at_approximate = true;
        store
            .replace_indexed_sessions(Provider::CursorCli, std::slice::from_ref(&session))
            .expect("write upgraded index");

        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::CursorCli)
                .expect("read upgraded index"),
            vec![session]
        );
    }

    #[test]
    fn legacy_trajectory_is_rechunked_with_truthful_unknown_coverage() {
        let temporary_directory = tempdir().expect("temporary directory");
        let database = temporary_directory.path().join("store.sqlite3");
        let connection = rusqlite::Connection::open(&database).expect("old store");
        connection
            .execute_batch(
                r#"CREATE TABLE session_trajectories (
                    id INTEGER PRIMARY KEY,
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    redacted_text TEXT NOT NULL,
                    source_updated_at INTEGER NOT NULL,
                    complete INTEGER NOT NULL,
                    indexed_at INTEGER NOT NULL,
                    UNIQUE (provider, session_id)
                );
                CREATE VIRTUAL TABLE session_trajectories_fts USING fts5(
                    redacted_text,
                    content = 'session_trajectories',
                    content_rowid = 'id'
                );
                CREATE TRIGGER session_trajectories_after_insert
                AFTER INSERT ON session_trajectories BEGIN
                    INSERT INTO session_trajectories_fts (rowid, redacted_text)
                    VALUES (new.id, new.redacted_text);
                END;
                CREATE TABLE bundles (
                    bundle_id TEXT PRIMARY KEY,
                    bundle_json TEXT NOT NULL,
                    saved_at INTEGER NOT NULL
                );
                INSERT INTO session_trajectories (
                    provider, session_id, redacted_text, source_updated_at, complete, indexed_at
                ) VALUES (
                    'codex', 'legacy', 'legacy migration searchable marker', 1, 1, 1
                );
                INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (
                    'synthetic-bundle',
                    '{"snapshot":{"session":{"provider":"codex","id":"legacy"}}}',
                    1
                );"#,
            )
            .expect("legacy schema");
        drop(connection);

        let store = Store::open(&database).expect("migrated store");
        let matches = store
            .search_session_trajectory_matches("migration searchable", 10)
            .expect("migrated search");
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].source_complete);
        assert!(!matches[0].complete);
        assert_eq!(matches[0].truncation_strategy, "legacy_unknown");
        assert_eq!(
            matches[0].indexed_byte_count,
            "legacy migration searchable marker".len()
        );
        let connection = store.connection.borrow();
        let parent_text = connection
            .query_row(
                "SELECT redacted_text FROM session_trajectories WHERE session_id = 'legacy'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("parent metadata");
        assert!(parent_text.is_empty());
        let old_fts_exists = connection
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM sqlite_master WHERE name = 'session_trajectories_fts'
                )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("old FTS state");
        assert!(!old_fts_exists);
        drop(connection);

        store
            .replace_indexed_sessions(Provider::Codex, &[])
            .expect("successful empty refresh");
        assert_eq!(
            store
                .search_session_trajectories("migration searchable", 10)
                .expect("bundle-protected migrated trajectory"),
            vec![SessionRef::new(Provider::Codex, "legacy")]
        );
    }

    #[test]
    fn failed_provider_check_preserves_cached_snapshot() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let cached = indexed_session(Provider::CursorIde, "cached", "Cached title");
        store
            .replace_indexed_sessions(Provider::CursorIde, std::slice::from_ref(&cached))
            .expect("initial index");

        store
            .mark_session_index_checked(Provider::CursorIde)
            .expect("record failed check");

        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::CursorIde)
                .expect("preserved index"),
            vec![cached]
        );
        assert!(
            store
                .session_index_checked_at(Provider::CursorIde)
                .expect("check state")
                .is_some()
        );
    }

    fn indexed_session(provider: Provider, id: &str, title: &str) -> IndexedSession {
        IndexedSession {
            source_revision: None,
            session: SessionRef::new(provider, id),
            title: Some(title.to_owned()),
            project_path: Some(PathBuf::from("/workspace/project")),
            git_branch: Some("main".to_owned()),
            created_at: None,
            updated_at: None,
            updated_at_approximate: false,
            event_count: 0,
        }
    }

    #[test]
    fn state_root_is_created_before_opening_its_store() {
        let temporary_directory = tempdir().expect("temporary directory");
        let state_root = temporary_directory.path().join("state");

        let _store = Store::open_state_root(&state_root).expect("store");

        assert!(state_root.is_dir());
        assert!(state_root.join("store.sqlite3").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn state_root_and_database_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary_directory = tempdir().expect("temporary directory");
        let state_root = temporary_directory.path().join("state");
        let _store = Store::open_state_root(&state_root).expect("store");

        assert_eq!(
            std::fs::metadata(&state_root)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(state_root.join("store.sqlite3"))
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_root_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary_directory = tempdir().expect("temporary directory");
        let real_root = temporary_directory.path().join("real");
        std::fs::create_dir(&real_root).expect("real state root");
        let linked_root = temporary_directory.path().join("linked");
        symlink(&real_root, &linked_root).expect("state-root symlink");

        assert!(Store::open_state_root(&linked_root).is_err());
    }

    #[test]
    fn default_state_root_is_user_home_dot_omnisession() {
        if std::env::var_os("OMNISESSION_HOME").is_some() {
            return;
        }
        let base_dirs = BaseDirs::new().expect("home directory");
        assert_eq!(
            state_root().expect("default state root"),
            base_dirs.home_dir().join(".omnisession")
        );
    }

    #[test]
    fn replacing_branch_head_preserves_prior_binding() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let task = store
            .create_or_get_task("handoff", temporary_directory.path())
            .expect("task");
        let first = SessionRef::new(Provider::Codex, "first");
        let second = SessionRef::new(Provider::Claude, "second");

        store
            .bind_session(task.id, "main", &first)
            .expect("first binding");
        store
            .bind_session(task.id, "main", &second)
            .expect("second binding");

        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .expect("head")
                .session,
            second
        );
        let connection = store.connection.borrow();
        let lineage = connection
            .prepare(
                "SELECT session_id, is_current FROM session_bindings
                 WHERE task_id = ?1 AND branch_name = ?2 ORDER BY id",
            )
            .expect("lineage query")
            .query_map(params![task.id, "main"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .expect("lineage rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("lineage values");
        assert_eq!(
            lineage,
            vec![("first".to_owned(), 0), ("second".to_owned(), 1)]
        );
    }

    #[test]
    fn handoff_lineage_orders_source_to_target_metadata() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let codex = SessionRef::new(Provider::Codex, "codex-1");
        let claude = SessionRef::new(Provider::Claude, "claude-1");
        let grok = SessionRef::new(Provider::Grok, "grok-1");

        store
            .record_handoff(
                &codex,
                &claude,
                TransferMode::SemanticHandoff,
                &json!({"status": "summarized"}),
            )
            .expect("first handoff");
        store
            .record_handoff(
                &claude,
                &grok,
                TransferMode::OfficialImport,
                &json!({"status": "preserved"}),
            )
            .expect("second handoff");

        let lineage = store.handoff_lineage().expect("handoff lineage");
        assert_eq!(lineage.len(), 2);
        assert_eq!(lineage[0].source, codex);
        assert_eq!(lineage[0].target, claude);
        assert_eq!(lineage[0].mode, TransferMode::SemanticHandoff);
        assert_eq!(lineage[1].source, claude);
        assert_eq!(lineage[1].target, grok);
        assert_eq!(lineage[1].mode, TransferMode::OfficialImport);
        assert!(lineage[0].created_at <= lineage[1].created_at);
    }

    #[test]
    fn combined_handoff_and_bind_commits_or_rolls_back_together() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let task = store
            .create_or_get_task("handoff", temporary_directory.path())
            .expect("task");
        let source = SessionRef::new(Provider::Codex, "source");
        let target = SessionRef::new(Provider::Claude, "target");
        let rejected_target = SessionRef::new(Provider::Grok, "rejected-target");
        store
            .bind_session(task.id, "main", &source)
            .expect("source binding");

        let binding = store
            .record_handoff_and_bind(
                task.id,
                "main",
                &source,
                &target,
                TransferMode::SemanticHandoff,
                &json!({"status": "summarized"}),
            )
            .expect("combined handoff");
        assert_eq!(binding.session, target);
        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding")
                .expect("head")
                .session,
            target
        );

        store
            .connection
            .borrow()
            .execute_batch(
                "
                CREATE TRIGGER reject_test_binding
                BEFORE INSERT ON session_bindings
                WHEN NEW.session_id = 'rejected-target'
                BEGIN
                    SELECT RAISE(ABORT, 'synthetic test failure');
                END;
                ",
            )
            .expect("test trigger");
        assert!(
            store
                .record_handoff_and_bind(
                    task.id,
                    "main",
                    &target,
                    &rejected_target,
                    TransferMode::SemanticHandoff,
                    &json!({"status": "unsupported"}),
                )
                .is_err()
        );

        assert_eq!(
            store
                .current_binding(task.id, "main")
                .expect("current binding after rollback")
                .expect("head")
                .session,
            target
        );
        let lineage = store.handoff_lineage().expect("handoff lineage");
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].source, source);
        assert_eq!(lineage[0].target, target);
    }

    #[test]
    fn portable_bundle_round_trips() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store_path = temporary_directory.path().join("store.sqlite3");
        let store = Store::open(&store_path).expect("store");
        let source = SessionRef::new(Provider::Codex, "session-1");
        let bundle = synthetic_bundle(source, temporary_directory.path());

        store.save_bundle(&bundle).expect("save bundle");
        assert_eq!(
            store
                .load_bundle(bundle.manifest.bundle_id)
                .expect("load bundle"),
            Some(bundle.clone())
        );
        drop(store);

        let reopened = Store::open(store_path).expect("reopen store");
        assert!(
            reopened
                .indexed_sessions_for_provider(Provider::Imported)
                .expect("read imported sources")
                .is_empty()
        );
        assert_eq!(
            reopened
                .bundle_ids_missing_imported_source()
                .expect("read bundles requiring durable source"),
            vec![bundle.manifest.bundle_id]
        );
    }

    #[test]
    fn saving_bundle_immediately_protects_existing_trajectory_from_refresh_pruning() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let cases = [
            ("replaceable", "replaceableprotectiontoken", false),
            ("insert-only", "insertonlyprotectiontoken", true),
        ];
        for (session_id, marker, insert_only) in cases {
            let session = SessionRef::new(Provider::Codex, session_id);
            store
                .upsert_session_trajectory(&session, marker, Utc::now(), true)
                .expect("index trajectory");
            let bundle = synthetic_bundle(session, temporary_directory.path());
            if insert_only {
                store.save_new_bundle(&bundle).expect("save new bundle");
            } else {
                store.save_bundle(&bundle).expect("save bundle");
            }
        }
        let unmatched_session = SessionRef::new(Provider::Codex, "unmatched");
        store
            .upsert_session_trajectory(
                &unmatched_session,
                "unmatchedsessiontoken",
                Utc::now(),
                true,
            )
            .expect("unmatched session trajectory");
        let unmatched_provider = SessionRef::new(Provider::Claude, "replaceable");
        store
            .upsert_session_trajectory(
                &unmatched_provider,
                "unmatchedprovidertoken",
                Utc::now(),
                true,
            )
            .expect("unmatched provider trajectory");

        store
            .replace_indexed_sessions(Provider::Codex, &[])
            .expect("successful empty refresh");
        store
            .replace_indexed_sessions(Provider::Claude, &[])
            .expect("successful other-provider refresh");

        for (session_id, marker, _) in cases {
            assert_eq!(
                store
                    .search_session_trajectories(marker, 10)
                    .expect("protected trajectory"),
                vec![SessionRef::new(Provider::Codex, session_id)]
            );
        }
        for marker in ["unmatchedsessiontoken", "unmatchedprovidertoken"] {
            assert!(
                store
                    .search_session_trajectories(marker, 10)
                    .expect("unprotected trajectory")
                    .is_empty()
            );
        }
    }

    fn synthetic_bundle(session: SessionRef, workspace: &Path) -> PortableBundle {
        let captured_at = Utc::now();
        PortableBundle {
            manifest: BundleManifest {
                schema_version: SCHEMA_VERSION.to_owned(),
                bundle_id: Uuid::new_v4(),
                created_at: captured_at,
                source: session.clone(),
                event_count: 0,
                redactions: Vec::new(),
            },
            snapshot: CanonicalSnapshot {
                schema_version: SCHEMA_VERSION.to_owned(),
                session,
                thread_id: Uuid::new_v4(),
                branch_id: Uuid::new_v4(),
                title: Some("bundle test".to_owned()),
                captured_at,
                workspace: WorkspaceSnapshot {
                    schema_version: SCHEMA_VERSION.to_owned(),
                    captured_at,
                    root: workspace.to_path_buf(),
                    current_dir: workspace.to_path_buf(),
                    git: GitState::default(),
                    instruction_files: Vec::new(),
                    environment_names: Vec::new(),
                    available_tools: Vec::new(),
                },
                events: Vec::new(),
            },
            fidelity: None,
        }
    }
}
