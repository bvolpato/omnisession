//! Local `SQLite` persistence for task selection, session lineage, and bundles.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, hash_map::RandomState},
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

mod cached_database_aliases;
mod physical_database_bodies;
pub use cached_database_aliases::CachedDatabaseAliasSource;
mod codex_legacy_home;
mod codex_title_recheck;
use codex_title_recheck::schedule_legacy_codex_title_proofs;
mod codex_workspace;
pub use codex_workspace::{CodexWorkspace, CodexWorkspaceIndex, CodexWorkspaceUpdate};

mod desktop_declarations;
mod discovery;
mod primary_legacy;
mod scoped_native_documents;
mod scoped_native_search;
pub mod search_query;

pub use discovery::{SessionDiscoveryCheckpoint, SessionDiscoveryClaim, SessionDiscoveryState};

const DATABASE_FILE_NAME: &str = "store.sqlite3";
const CODEX_SCAN_KEY_PREFIX: &str = "codex:";
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
const FIRST_PRIMARY_LEGACY_CHUNK: i64 = 1 << 31;
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

// A failed replacement hides the old native text until that rollout is selected again.
const CONFLICTING_CODEX_FAILURE: &str = "trajectories.provider = 'codex'
    AND trajectories.origin = 'native' AND failed.provider = trajectories.provider
    AND failed.session_id = trajectories.session_id AND failed.source_file_id IS NOT NULL
    AND (failed.source_file_id IS NOT trajectories.source_file_id
        OR failed.source_contents_id IS NOT trajectories.source_contents_id
        OR failed.source_updated_at IS NOT trajectories.source_updated_at
        OR (failed.source_updated_submillis IS NOT NULL
            AND failed.source_updated_submillis IS NOT trajectories.source_updated_submillis))";

// Discovery can select a replacement before its first transcript read succeeds.
const CONFLICTING_CODEX_CATALOG: &str = "trajectories.provider = 'codex'
    AND trajectories.origin = 'native' AND catalog.provider = trajectories.provider
    AND catalog.session_id = trajectories.session_id AND catalog.source_file_id IS NOT NULL
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
            AND (chunks.chunk_index < {FIRST_PRIMARY_LEGACY_CHUNK} OR NOT EXISTS (
                SELECT 1 FROM primary_legacy_chunk_updates WHERE trajectory_id=trajectories.id))
         INNER JOIN eligible
             ON eligible.provider = trajectories.provider
            AND eligible.session_id = trajectories.session_id
            AND (eligible.file IS NULL OR (eligible.file = hex(trajectories.source_file_id)
                 AND eligible.contents = hex(trajectories.source_contents_id)))
            AND (eligible.file IS NOT NULL OR NOT EXISTS (
                 SELECT 1 FROM session_index AS catalog WHERE {CONFLICTING_CODEX_CATALOG}))
            AND NOT EXISTS (SELECT 1 FROM trajectory_index_failures AS failed
                            WHERE {CONFLICTING_CODEX_FAILURE})"
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
        source_updated_submillis, provider_limited, native_summary, source_file_id, missing_ancestor_id, source_contents_id,
        derived_title_is_verified, derived_title_is_local_opener
    ) SELECT ?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?16, ?17, ?19, ?20, ?21, ?22,
             ?25, CASE WHEN ?25 THEN ?27 END
    -- A direct database read can return the same document even when its event time is unreliable.
    -- Check the projection before INSERT triggers run. The transaction still verifies the source and clears failures.
    WHERE NOT ?18 OR NOT EXISTS (
        SELECT 1 FROM session_trajectories
        WHERE provider = ?1 AND session_id = ?2
          AND (content_hash, source_updated_at, source_complete, complete, source_byte_count, indexed_byte_count,
               truncation_strategy, origin, document_version, derived_title, source_updated_submillis,
               provider_limited, native_summary, source_file_id, missing_ancestor_id, source_contents_id,
               derived_title_is_verified, derived_title_is_local_opener)
              IS (?3, ?4, ?5, ?6, ?8, ?9, ?10, ?11, ?13, ?14, ?16, ?17, ?19, ?20, ?21, ?22,
                  ?25, CASE WHEN ?25 THEN ?27 END)
    )
    ON CONFLICT (provider, session_id) DO UPDATE SET
        content_hash = excluded.content_hash,
        source_file_id = excluded.source_file_id,
        missing_ancestor_id = excluded.missing_ancestor_id,
        source_contents_id = excluded.source_contents_id,
        source_updated_at = excluded.source_updated_at,
        source_updated_submillis = excluded.source_updated_submillis,
        source_complete = excluded.source_complete,
        provider_limited = excluded.provider_limited,
        native_summary = excluded.native_summary,
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
        (derived_title, derived_title_is_verified, derived_title_is_local_opener) = (
            SELECT CASE WHEN replace_title THEN excluded.derived_title
                        ELSE session_trajectories.derived_title END,
                   CASE WHEN replace_title THEN excluded.derived_title_is_verified
                            OR (excluded.derived_title IS NOT NULL AND session_trajectories.derived_title_is_verified
                                AND excluded.source_file_id IS session_trajectories.source_file_id
                                AND excluded.derived_title IS session_trajectories.derived_title)
                        ELSE session_trajectories.derived_title_is_verified END,
                   CASE WHEN NOT coalesce(replace_title, 0) THEN session_trajectories.derived_title_is_local_opener
                        WHEN same_title AND session_trajectories.derived_title_is_verified AND (
                             NOT excluded.derived_title_is_verified OR
                             (session_trajectories.derived_title_is_local_opener IS 0 AND ?27 AND NOT ?29))
                          THEN session_trajectories.derived_title_is_local_opener
                        ELSE excluded.derived_title_is_local_opener END
            FROM (SELECT
                excluded.source_file_id IS session_trajectories.source_file_id
                AND excluded.derived_title IS NOT NULL AND excluded.derived_title IS session_trajectories.derived_title AS same_title,
                CASE WHEN excluded.provider <> 'codex' OR excluded.origin <> 'native' THEN
                          excluded.derived_title IS NOT NULL
                          OR (?18 AND (excluded.native_summary <> session_trajectories.native_summary
                              OR ((session_trajectories.source_file_id IS NOT NULL OR excluded.provider IN ('antigravity','antigravity-desktop','antigravity-ide'))
                                  AND excluded.source_file_id IS NOT session_trajectories.source_file_id)))
                          OR (excluded.source_complete AND excluded.truncation_strategy IN (
                              'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'
                          ) AND (?15 OR excluded.document_version > session_trajectories.document_version
                                 OR NOT session_trajectories.source_complete))
                     WHEN ?23 AND excluded.source_file_id IS NOT NULL
                          AND excluded.source_file_id IS NOT session_trajectories.source_file_id THEN 1
                     WHEN ?23 AND ?29 THEN 1
                     WHEN ?28 THEN session_trajectories.origin = 'native'
                          AND excluded.source_file_id = session_trajectories.source_file_id
                          AND ?26 = session_trajectories.derived_title
                          AND session_trajectories.derived_title_is_local_opener IS NOT 0
                     WHEN ?27 THEN session_trajectories.derived_title_is_local_opener IS NOT 0
                     ELSE (NOT ?23 AND excluded.derived_title IS NOT NULL)
                OR (?26 = session_trajectories.derived_title
                    AND NOT session_trajectories.derived_title_is_verified
                    AND session_trajectories.origin = 'native'
                    AND excluded.source_file_id = session_trajectories.source_file_id)
                END AS replace_title)
        )
    WHERE excluded.document_version >= session_trajectories.document_version
      -- A new Antigravity reader can calculate a different receipt for the same transcript.
      AND (excluded.provider <> 'antigravity' OR excluded.origin <> 'native'
           OR session_trajectories.origin <> 'native'
           OR excluded.document_version = session_trajectories.document_version
           OR NOT (session_trajectories.source_complete OR session_trajectories.provider_limited)
           OR excluded.source_complete OR excluded.provider_limited OR ?30)
      AND CASE
          WHEN ?30 AND excluded.document_version > session_trajectories.document_version THEN 1
          WHEN excluded.provider = 'codex' THEN 1
          WHEN excluded.source_file_id IS NULL THEN session_trajectories.source_file_id IS NULL
          WHEN session_trajectories.source_file_id IS NULL THEN ?18 AND (
              NOT session_trajectories.source_complete OR excluded.source_complete OR excluded.provider_limited)
          WHEN excluded.source_file_id IS NOT session_trajectories.source_file_id
            OR excluded.source_contents_id IS NOT session_trajectories.source_contents_id
            OR excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id THEN ?18
          ELSE ?18 AND (excluded.provider_limited OR
               (excluded.source_complete, excluded.provider_limited) >=
               (session_trajectories.source_complete, session_trajectories.provider_limited))
          END
      AND (session_trajectories.native_summary = 0 OR ?18)
      AND CASE
      WHEN ?30 AND excluded.document_version > session_trajectories.document_version THEN 1
      WHEN ?23 AND excluded.provider = 'codex' AND excluded.origin = 'native'
           AND session_trajectories.origin = 'native' AND excluded.source_file_id IS NOT NULL
           AND (excluded.source_file_id IS NOT session_trajectories.source_file_id
               OR (excluded.source_contents_id IS NOT NULL
                   AND excluded.source_contents_id IS NOT session_trajectories.source_contents_id)
               OR (session_trajectories.missing_ancestor_id IS NOT NULL
                   AND excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id)) THEN 1
      WHEN ?18 AND excluded.source_file_id IS NOT NULL AND (
          excluded.source_file_id IS NOT session_trajectories.source_file_id
          OR excluded.source_contents_id IS NOT session_trajectories.source_contents_id
          OR excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id) THEN 1
      WHEN ?18 AND excluded.native_summary <> session_trajectories.native_summary THEN 1
      ELSE (
          (excluded.source_complete, excluded.provider_limited) >=
              (session_trajectories.source_complete, session_trajectories.provider_limited)
          OR (excluded.provider_limited AND (?15
              OR (excluded.source_updated_at, excluded.source_updated_submillis) >=
                 (session_trajectories.source_updated_at, coalesce(session_trajectories.source_updated_submillis, 0))))
          OR (excluded.document_version = session_trajectories.document_version
              AND (NOT ?15 OR session_trajectories.source_updated_submillis IS NOT NULL))
      ) AND (
       excluded.document_version > session_trajectories.document_version
       OR (excluded.source_updated_at, excluded.source_updated_submillis)
           > (session_trajectories.source_updated_at, coalesce(session_trajectories.source_updated_submillis, 0))
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
                           (excluded.complete = session_trajectories.complete OR ((?18 OR ?24) AND excluded.source_complete))
                           AND (
                               excluded.content_hash <> session_trajectories.content_hash
                               OR excluded.source_byte_count <>
                                   session_trajectories.source_byte_count
                               OR excluded.indexed_byte_count <>
                                   session_trajectories.indexed_byte_count
                               OR excluded.truncation_strategy <>
                                   session_trajectories.truncation_strategy
                               OR excluded.origin <> session_trajectories.origin
                               OR excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id
                               OR (?23 AND ?29 AND excluded.derived_title IS NOT session_trajectories.derived_title)
                           )
                       )
                   )
           )
       )
    )) END
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

/// Identifies one provider-wide check of a native home by a specific reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDiscoveryAttempt {
    provider: Provider,
    version: u32,
    home: PathBuf,
    id: Uuid,
}

impl SessionDiscoveryAttempt {
    /// Returns the native home selected when the discovery began.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }
}

/// Cached provider metadata used to render session discovery without transcript reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedSession {
    pub session: SessionRef,
    /// Physical source backing this catalog entry, when identified by the provider.
    pub source_revision: Option<SourceRevision>,
    pub title: Option<String>,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_at_approximate: bool,
    pub event_count: usize,
}

/// Extent of the native catalog observed by one listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryScope {
    /// The provider confirmed the entire catalog, including any deletions.
    CompleteProvider,
    /// The provider returned the rows it could read across all workspaces.
    PartialProvider,
    /// Desktop enumerated its databases and archives, but could not read their optional summaries.
    DesktopDatabasesWithoutSummaries,
    /// These Desktop databases were found, but their archive annotations could not be verified.
    DesktopUnverifiedArchives {
        session_ids: Vec<String>,
        summaries_unavailable: bool,
    },
    /// Desktop read its summary file, but some listed databases had no summary entry.
    DesktopPartialSummaries {
        missing_summary_ids: Vec<String>,
        unverified_archive_ids: Vec<String>,
    },
    /// The provider was asked for a single workspace.
    Workspace,
}

enum MissingDesktopSummaries {
    None,
    All,
    Selected(std::collections::HashSet<String>),
}

impl MissingDesktopSummaries {
    fn contains(&self, id: &str) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Selected(ids) => ids.contains(id),
        }
    }
}

impl DiscoveryScope {
    /// Supplies the selected file for each catalog row in its original order.
    #[must_use]
    pub fn with_source_files(self, file_at: &dyn Fn(usize) -> Option<Uuid>) -> CatalogListing<'_> {
        CatalogListing {
            scope: self,
            file_at: Some(file_at),
            archived_desktop_ids: &[],
            database_alias_counts: None,
        }
    }

    fn desktop_observations(
        self,
        provider: Provider,
    ) -> Result<(Vec<String>, MissingDesktopSummaries)> {
        let observed = match self {
            Self::DesktopUnverifiedArchives {
                session_ids,
                summaries_unavailable,
            } => (
                session_ids,
                if summaries_unavailable {
                    MissingDesktopSummaries::All
                } else {
                    MissingDesktopSummaries::None
                },
            ),
            Self::DesktopPartialSummaries {
                missing_summary_ids,
                unverified_archive_ids,
            } => (
                unverified_archive_ids,
                MissingDesktopSummaries::Selected(missing_summary_ids.into_iter().collect()),
            ),
            Self::DesktopDatabasesWithoutSummaries => (Vec::new(), MissingDesktopSummaries::All),
            _ => return Ok((Vec::new(), MissingDesktopSummaries::None)),
        };
        if desktop_declarations::table(provider).is_none() {
            return Err(StoreError::InvalidSessionReference);
        }
        Ok(observed)
    }
}

type DatabaseAliasCounts<'a> = &'a dyn Fn(&str) -> Option<usize>;

/// A discovery scope and any physical file identities observed in the same listing.
pub struct CatalogListing<'a> {
    scope: DiscoveryScope,
    file_at: Option<&'a dyn Fn(usize) -> Option<Uuid>>,
    archived_desktop_ids: &'a [String],
    database_alias_counts: Option<DatabaseAliasCounts<'a>>,
}

impl<'a> CatalogListing<'a> {
    /// Records the IDs whose Desktop archive annotations were verified in this listing.
    #[must_use]
    pub fn with_archived_desktop_ids(mut self, ids: &'a [String]) -> Self {
        self.archived_desktop_ids = ids;
        self
    }

    /// Supplies counts declared by verified IDE database summaries in this listing.
    #[must_use]
    pub fn with_database_alias_counts(mut self, counts: &'a dyn Fn(&str) -> Option<usize>) -> Self {
        self.database_alias_counts = Some(counts);
        self
    }
}

impl From<DiscoveryScope> for CatalogListing<'_> {
    fn from(scope: DiscoveryScope) -> Self {
        Self {
            scope,
            file_at: None,
            archived_desktop_ids: &[],
            database_alias_counts: None,
        }
    }
}

/// The catalog membership seen before a scan of the active Codex home.
#[derive(Default)]
pub struct CodexCatalogInventory {
    /// Raw rows used to detect concurrent catalog changes.
    pub observed: Vec<IndexedSession>,
    /// Names the store already trusts for the active home.
    pub observable_names: HashMap<SessionRef, String>,
    /// Legacy names whose source and prior native assignment can be checked.
    unscoped: HashMap<SessionRef, usize>,
    unstamped_documents: HashMap<SessionRef, String>,
}

impl CodexCatalogInventory {
    /// Reports whether this inventory contains legacy names that a native scan could corroborate.
    #[must_use]
    pub fn has_legacy_codex_names(&self) -> bool {
        !self.unscoped.is_empty()
    }

    /// Returns the stored physical revision for a recoverable legacy name.
    #[must_use]
    pub fn legacy_codex_source(&self, session: &SessionRef) -> Option<SourceRevision> {
        self.observed
            .get(*self.unscoped.get(session)?)?
            .source_revision
    }

    /// Returns the stored spelling when the historical native assignment matches this legacy row.
    #[must_use]
    pub fn corroborated_legacy_codex_name(
        &self,
        selected: &IndexedSession,
        prior_assignment: &str,
    ) -> Option<&str> {
        let old = self.observed.get(*self.unscoped.get(&selected.session)?)?;
        corroborated_legacy_name(old, selected, prior_assignment)
    }
}

#[derive(Clone, Copy)]
struct CatalogNameObservations<'a> {
    observed: &'a [IndexedSession],
    complete: bool,
    prior_assignments: &'a HashMap<String, String>,
    unstamped_documents: Option<&'a HashMap<SessionRef, String>>,
}

#[derive(Clone, Copy)]
struct SelectedMetadata<'a> {
    selected: &'a IndexedSession,
    observation: &'a SelectedSessionObservation,
    name_observed: bool,
}

impl SelectedMetadata<'_> {
    fn reconcile_name(self, transaction: &Transaction<'_>) -> Result<()> {
        if self.selected.title.is_none()
            && let Some(prior) = self.observation.metadata.as_ref()
        {
            if !same_codex_catalog_source(prior, self.selected) {
                remember_provider_name(transaction, prior, self.selected)?;
            } else if self.name_observed {
                withdraw_catalog_name(transaction, prior, false)?;
            }
        } else if self.selected.title.is_none() && self.observation.untrusted.is_some() {
            withdraw_other_home_name(transaction, self.selected, false)?;
        }
        if self.name_observed {
            resolve_pending_catalog_title(transaction, self.selected, false)?;
        }
        Ok(())
    }

    fn needs_catalog_update(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        content_hash: &[u8],
    ) -> Result<bool> {
        if cached_index_failure(transaction, session)? != self.observation.failure {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let (current, untrusted) = selected_catalog_rows(transaction, session)?;
        let existing = current.as_ref().map(|row| &row.metadata);
        if existing != Some(self.selected)
            && (existing != self.observation.metadata.as_ref()
                || untrusted != self.observation.untrusted)
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let hash = cached_content_hash(transaction, session)?;
        if hash != self.observation.content_hash && hash.as_deref() != Some(content_hash) {
            return Err(StoreError::TrajectorySourceChanged);
        }
        Ok(existing != Some(self.selected))
    }

    fn publish(self, transaction: &Transaction<'_>, update_catalog: bool) -> Result<()> {
        if update_catalog {
            write_indexed_session(transaction, self.selected)?;
        }
        if let Some(failure) = self.observation.failure {
            clear_observed_failure(transaction, &self.selected.session, failure)?;
        }
        Ok(())
    }
}

#[derive(Eq, PartialEq)]
struct UntrustedCatalogRow {
    row: IndexedSession,
    home: Option<String>,
    recoverable: bool,
}

/// The document and catalog state before an explicit selected-source read.
pub struct SelectedSessionObservation {
    pub state: Option<TrajectoryIndexState>,
    pub metadata: Option<IndexedSession>,
    pub failure: Option<TrajectoryIndexFailure>,
    untrusted: Option<UntrustedCatalogRow>,
    content_hash: Option<Vec<u8>>,
}

impl SelectedSessionObservation {
    /// Returns the catalog name for this rollout before its first document has been indexed.
    #[must_use]
    pub fn unindexed_codex_name(&self, selected: &IndexedSession) -> Option<&str> {
        unindexed_codex_name(self.metadata.as_ref()?, selected, self.state)
    }

    /// Reports whether the active native sidecar could corroborate a legacy name for this file.
    #[must_use]
    pub fn legacy_codex_name_candidate(&self, selected: &IndexedSession) -> bool {
        self.untrusted.as_ref().is_some_and(|prior| {
            prior.recoverable && same_legacy_catalog_file(&prior.row, selected)
        })
    }

    /// Returns the stored name corroborated by a prior assignment for this physical native thread.
    #[must_use]
    pub fn corroborated_legacy_codex_name(
        &self,
        selected: &IndexedSession,
        prior_assignment: &str,
    ) -> Option<&str> {
        let old = self.untrusted.as_ref().filter(|row| row.recoverable)?;
        corroborated_legacy_name(&old.row, selected, prior_assignment)
    }
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
    /// A verified native catalog summary for a session with zero steps and no history file.
    NativeSummary,
    ImportedBundle,
}

impl SessionTrajectoryOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Native | Self::NativeSummary => "native",
            Self::ImportedBundle => "imported_bundle",
        }
    }
}

/// What replay of the selected ordinary Codex rollout established about its local opener.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LocalOpenerEvidence {
    #[default]
    Unknown,
    /// The first surviving message and any title it supplies are known.
    Current,
    /// The first visible message is known, but the provider name is unresolved and the message may be inherited.
    VisiblePrefix,
    /// The former opener was removed. The surviving replacement could not be read.
    Withdrawn,
    /// Replay verified an ordinary opener against its one-time legacy receipt.
    RecheckedCurrent,
    /// Replay proved the opening message in the one-time legacy receipt was removed.
    RecheckedWithdrawn,
    /// A verified rollback is waiting for the provider name index to confirm the session is unnamed.
    DeferredLegacyName,
}

impl LocalOpenerEvidence {
    const fn rechecks_legacy_title(self) -> bool {
        matches!(self, Self::RecheckedCurrent | Self::RecheckedWithdrawn)
    }
}

/// A saved Codex opener and the evidence needed to correct its legacy provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyCodexOpenerCandidate {
    pub title: String,
    /// The source must contain a valid persisted tool catalog as well as the exact old opener.
    pub requires_tool_catalog: bool,
}

/// One redacted search document with coverage, provenance, and version metadata.
#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Source coverage and title evidence are independent properties."
)]
pub struct TrajectoryDocument<'a> {
    pub redacted_text: &'a str,
    pub database_step_proof: Option<&'a [Uuid]>,
    pub desktop_projection: Option<&'a omnis_core::DesktopProjection>,
    pub legacy_utf8_text: Option<&'a omnis_core::LegacyUtf8Projection>,
    pub source_revision: SourceRevision,
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
    /// The provider named the session or the read verified its opening message.
    pub title_is_verified: bool,
    pub local_opener: LocalOpenerEvidence,
    /// The exact saved title that a separate local history read verified for withdrawal or repair.
    /// The indexed text can still come from a preview.
    pub withdrawn_title: Option<&'a str>,
    /// Whether this read establishes the first title, including when the session has none.
    pub title_authoritative: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogObservation {
    Unchecked,
    Missing,
    Verified { home: [u8; 32], metadata: [u8; 32] },
}

impl CatalogObservation {
    fn read(connection: &Connection, session: &SessionRef) -> Result<Self> {
        let mut statement = connection
            .prepare_cached(
                "SELECT home, json_array(title, project_path, git_branch, created_at, updated_at,
                         updated_at_approximate, event_count)
             FROM session_index JOIN native_catalog_homes USING(provider,session_id)
             WHERE provider = ?1 AND session_id = ?2 AND verified",
            )
            .map_err(database_error)?;
        statement
            .query_row(params![session.provider.to_string(), session.id], |row| {
                Ok(Self::Verified {
                    home: Sha256::digest(row.get_ref(0)?.as_blob()?).into(),
                    metadata: Sha256::digest(row.get_ref(1)?.as_str()?).into(),
                })
            })
            .optional()
            .map_err(database_error)
            .map(|row| row.unwrap_or(Self::Missing))
    }
}

/// The indexed document and any Antigravity failure present before the native read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrajectoryDocumentObservation {
    catalog: CatalogObservation,
    document: Option<[u8; 32]>,
    failure: Option<[u8; 32]>,
    desktop_steps: Option<usize>,
    database_contents: Option<Uuid>,
    desktop_generation: Option<i64>,
    incomplete_native_history: bool,
}

/// Restricts a write to the source state the reader saw before opening the transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectorySourceGuard {
    /// The provider orders updates by source time.
    NewerSource,
    /// A restored file may have any modification time. Only replace the source the reader saw.
    Observed(Option<TrajectoryIndexState>),
    /// The provider's event time can repeat or move backwards after a restore.
    ObservedDocument(TrajectoryDocumentObservation),
}

impl TrajectorySourceGuard {
    /// Whether the search reader verifies both the native database or transcript and its catalog home.
    #[must_use]
    pub const fn uses_catalog_home(provider: Provider) -> bool {
        matches!(
            provider,
            Provider::Antigravity | Provider::AntigravityDesktop | Provider::AntigravityIde
        )
    }

    /// Binds a background write to the native home verified while reading the source.
    #[must_use]
    pub fn for_published_catalog_home(mut self, home: &Path) -> Self {
        if let Self::ObservedDocument(prior) = &mut self {
            let actual: [u8; 32] = Sha256::digest(home.as_os_str().as_encoded_bytes()).into();
            if !matches!(prior.catalog, CatalogObservation::Verified {home, ..} if home == actual) {
                prior.catalog = CatalogObservation::Missing;
            }
        }
        self
    }

    /// Marks a readable native prefix whose database or WAL still has missing steps.
    #[must_use]
    pub fn with_incomplete_native_history(mut self, incomplete: bool) -> Self {
        if let Self::ObservedDocument(observed) = &mut self {
            observed.incomplete_native_history = incomplete;
        }
        self
    }

    /// Binds the read to the observed database and WAL independently of the optional summary.
    #[must_use]
    pub fn with_database_contents(mut self, contents: Option<Uuid>) -> Self {
        if let Self::ObservedDocument(observed) = &mut self {
            observed.database_contents = contents;
        }
        self
    }

    /// Records the step count from the Desktop summary observed with this native read.
    #[must_use]
    pub fn with_catalog_step_count(
        mut self,
        provider: Provider,
        count: usize,
        summaries_unavailable: bool,
    ) -> Self {
        if provider == Provider::AntigravityDesktop
            && !summaries_unavailable
            && let Self::ObservedDocument(observation) = &mut self
        {
            observation.desktop_steps = Some(count);
        }
        self
    }

    /// Records the verified summary count for an IDE session read from a Desktop-format database.
    #[must_use]
    pub fn with_database_alias_step_count(mut self, count: usize) -> Self {
        if let Self::ObservedDocument(observation) = &mut self {
            observation.desktop_steps = Some(count);
        }
        self
    }

    /// Whether a native reader identifies a search source by its file modification time.
    #[must_use]
    pub const fn uses_file_time(provider: Provider) -> bool {
        matches!(
            provider,
            Provider::Codex | Provider::Claude | Provider::Pi | Provider::OhMyPi
        )
    }

    fn check(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        document: Option<&TrajectoryDocument<'_>>,
    ) -> Result<()> {
        let prior = match self {
            Self::NewerSource => return Ok(()),
            Self::ObservedDocument(prior) => {
                match prior.catalog {
                    CatalogObservation::Missing => return Err(StoreError::TrajectorySourceChanged),
                    CatalogObservation::Verified { .. }
                        if CatalogObservation::read(transaction, session)? != prior.catalog =>
                    {
                        return Err(StoreError::TrajectorySourceChanged);
                    }
                    _ => {}
                }
                return if document_receipt(transaction, session)? == prior.document {
                    Ok(())
                } else {
                    Err(StoreError::TrajectorySourceChanged)
                };
            }
            Self::Observed(prior) => prior,
        };
        let mut statement = transaction
            .prepare_cached(
                "SELECT source_updated_at, source_updated_submillis, source_file_id, missing_ancestor_id, source_contents_id
                 FROM session_trajectories WHERE provider = ?1 AND session_id = ?2",
            )
            .map_err(database_error)?;
        let current = statement
            .query_row(params![session.provider.to_string(), session.id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    source_revision_from_row(row, 2)?,
                ))
            })
            .optional()
            .map_err(database_error)?;
        let expected = prior.map(|state| {
            (
                state.source_updated_at.timestamp_millis(),
                state
                    .source_time_precise
                    .then(|| source_submillis(session.provider, state.source_updated_at)),
                state.source_revision,
            )
        });
        // A concurrent reader may still improve coverage of the same rollout.
        if current == expected
            || document.is_some_and(|read| {
                current
                    == Some((
                        read.source_updated_at.timestamp_millis(),
                        Some(source_submillis(session.provider, read.source_updated_at)),
                        read.source_revision,
                    ))
            })
        {
            Ok(())
        } else {
            Err(StoreError::TrajectorySourceChanged)
        }
    }
}

/// Hash the indexed row before a native read. Older clients can also update these fields.
fn document_receipt(connection: &Connection, session: &SessionRef) -> Result<Option<[u8; 32]>> {
    connection
        .prepare_cached(
            "SELECT json_array(id, indexed_at, hex(content_hash), source_updated_at,
                       source_updated_submillis, source_complete, provider_limited, complete,
                       document_version, source_byte_count, indexed_byte_count,
                       truncation_strategy, origin, derived_title, derived_title_is_verified,
                       derived_title_is_local_opener, native_summary,
                       hex(source_file_id), hex(source_contents_id), hex(missing_ancestor_id))
             FROM session_trajectories WHERE provider = ?1 AND session_id = ?2",
        )
        .map_err(database_error)?
        .query_row(params![session.provider.to_string(), session.id], |row| {
            Ok(Sha256::digest(row.get::<_, String>(0)?).into())
        })
        .optional()
        .map_err(database_error)
}

/// Indexing state of one cached search document, used to skip unchanged sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "These fields record independent properties of the indexed source."
)]
pub struct TrajectoryIndexState {
    pub source_revision: SourceRevision,
    pub source_updated_at: DateTime<Utc>,
    /// False for historical rows written before the index recorded file time below a millisecond.
    pub source_time_precise: bool,
    pub source_complete: bool,
    /// The adapter exhausted its full-read budget and returned its best available preview.
    pub provider_limited: bool,
    /// The native catalog was the only source of this document.
    pub native_summary: bool,
    pub document_version: u32,
}

/// An indexed title and the physical source that supplied it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedTrajectoryTitle {
    pub session: SessionRef,
    pub title: Option<String>,
    pub source_revision: SourceRevision,
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
    /// Selected provider file at the time of the failed read.
    pub source_file: Option<Uuid>,
    /// Contents of the selected file at the time of the failed read.
    pub source_contents: Option<Uuid>,
    /// Source update time discovered when the read failed, when the provider reports one.
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Whether the indexer retained the full precision of the source time.
    pub source_time_precise: bool,
    /// The failed candidate identified a native catalog summary without a history file.
    pub native_summary: bool,
    /// Search document version of the indexer whose read failed.
    pub document_version: u32,
}

struct PreparedTrajectoryWrite<'session, 'document> {
    session: &'session SessionRef,
    document: TrajectoryDocument<'document>,
    workspace: Option<&'document CodexWorkspaceUpdate>,
    content_hash: Vec<u8>,
    guard: TrajectorySourceGuard,
    source_complete: bool,
    complete: bool,
    source_byte_count: i64,
    indexed_byte_count: i64,
}

impl PreparedTrajectoryWrite<'_, '_> {
    fn upsert_parent(
        &self,
        statement: &mut rusqlite::Statement<'_>,
        title: &TrajectoryTitleWrite<'_>,
        selected: bool,
        verified_partial: bool,
    ) -> Result<Option<i64>> {
        statement
            .query_row(
                params![
                    self.session.provider.to_string(),
                    self.session.id,
                    self.content_hash,
                    self.document.source_updated_at.timestamp_millis(),
                    i64::from(self.source_complete),
                    i64::from(self.complete),
                    now_timestamp(),
                    self.source_byte_count,
                    self.indexed_byte_count,
                    self.document.truncation_strategy,
                    self.document.origin.as_str(),
                    i64::from(self.document.origin == SessionTrajectoryOrigin::ImportedBundle),
                    i64::from(self.document.document_version),
                    title.current,
                    self.guard != TrajectorySourceGuard::NewerSource,
                    source_submillis(self.session.provider, self.document.source_updated_at),
                    self.document.provider_limited,
                    self.document.origin != SessionTrajectoryOrigin::ImportedBundle
                        && matches!(self.guard, TrajectorySourceGuard::ObservedDocument(_)),
                    self.document.origin == SessionTrajectoryOrigin::NativeSummary,
                    self.document.source_revision.file.map(Uuid::into_bytes),
                    self.document
                        .source_revision
                        .missing_ancestor
                        .map(Uuid::into_bytes),
                    self.document.source_revision.contents.map(Uuid::into_bytes),
                    matches!(self.guard, TrajectorySourceGuard::Observed(_)),
                    selected,
                    title.verified,
                    title.withdrawn,
                    title.local == LocalOpenerEvidence::Current,
                    title.local == LocalOpenerEvidence::Withdrawn,
                    title.authoritative,
                    verified_partial,
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)
    }

    fn may_publish_unchanged_locator(&self, transaction: &Transaction<'_>) -> Result<bool> {
        let revision = self.document.source_revision;
        transaction.query_row(
            "SELECT CASE
                WHEN document_version = ?4 AND native_summary = ?3
                 AND source_file_id = ?5 AND source_contents_id = ?6 AND missing_ancestor_id IS ?7
                 AND (source_complete OR (content_hash = ?8 AND provider_limited >= ?9 AND NOT ?10))
                    THEN 1
                WHEN document_version < ?4 AND (source_complete OR provider_limited)
                 AND NOT ?9 AND NOT ?10 THEN 0
             END
             FROM session_trajectories WHERE provider = ?1 AND session_id = ?2 AND origin = 'native'",
            params![self.session.provider.to_string(), self.session.id,
                    self.document.origin == SessionTrajectoryOrigin::NativeSummary,
                    self.document.document_version, revision.file.map(Uuid::into_bytes),
                    revision.contents.map(Uuid::into_bytes), revision.missing_ancestor.map(Uuid::into_bytes),
                    self.content_hash, self.document.provider_limited, self.source_complete],
            |row| row.get::<_, Option<bool>>(0),
        ).optional().map_err(database_error)?.flatten().ok_or(StoreError::TrajectorySourceChanged)
    }

    fn reuses_revision_chunks(&self, transaction: &Transaction<'_>) -> Result<bool> {
        let revision = self.document.source_revision;
        if revision.file.is_none()
            || !matches!(self.guard, TrajectorySourceGuard::ObservedDocument(_))
        {
            return Ok(false);
        }
        // Version 4 already used this chunk layout. A verified read can add the missing
        // source revision to a released Antigravity document without rewriting equal text.
        transaction
            .prepare_cached(
                "SELECT EXISTS (
                 SELECT 1 FROM session_trajectories WHERE provider = ?1 AND session_id = ?2
                   AND content_hash = ?4 AND (
                     (document_version = ?3 AND (source_file_id IS NOT ?5
                        OR source_contents_id IS NOT ?6 OR missing_ancestor_id IS NOT ?7))
                     OR (provider = 'antigravity' AND origin = 'native' AND ?6 IS NOT NULL
                        AND document_version BETWEEN 4 AND ?3)
                   )
             )",
            )
            .and_then(|mut statement| {
                statement.query_row(
                    params![
                        self.session.provider.to_string(),
                        self.session.id,
                        self.document.document_version,
                        self.content_hash,
                        revision.file.map(Uuid::into_bytes),
                        revision.contents.map(Uuid::into_bytes),
                        revision.missing_ancestor.map(Uuid::into_bytes)
                    ],
                    |row| row.get(0),
                )
            })
            .map_err(database_error)
    }
}

#[derive(Default)]
struct TrajectoryTitleWrite<'a> {
    current: Option<&'a str>,
    withdrawn: Option<&'a str>,
    local: LocalOpenerEvidence,
    verified: bool,
    authoritative: bool,
}

impl PreparedTrajectoryWrite<'_, '_> {
    fn carry_pending_catalog_title(&self, transaction: &Transaction<'_>) -> Result<()> {
        let current = self.document.source_revision;
        if let TrajectorySourceGuard::Observed(Some(prior)) = self.guard
            && prior.source_revision.file == current.file
            && prior.source_revision.contents != current.contents
        {
            carry_pending_catalog_title(transaction, self.session, prior.source_revision, current)?;
        }
        Ok(())
    }

    fn title_evidence(
        &self,
        transaction: &Transaction<'_>,
        observed: bool,
    ) -> Result<TrajectoryTitleWrite<'_>> {
        let document = &self.document;
        if self.session.provider != Provider::Codex
            || document.origin != SessionTrajectoryOrigin::Native
        {
            return Ok(TrajectoryTitleWrite {
                current: document.derived_title,
                ..TrajectoryTitleWrite::default()
            });
        }
        if document.local_opener == LocalOpenerEvidence::DeferredLegacyName {
            return Ok(TrajectoryTitleWrite::default());
        }
        let revision = document.source_revision;
        let stamped = observed && revision.file.is_some() && revision.contents.is_some();
        let withdrawn = document.withdrawn_title.filter(|_| stamped);
        let local = match document.local_opener {
            LocalOpenerEvidence::Current | LocalOpenerEvidence::RecheckedCurrent if stamped => {
                LocalOpenerEvidence::Current
            }
            LocalOpenerEvidence::Withdrawn | LocalOpenerEvidence::RecheckedWithdrawn
                if withdrawn.is_some() =>
            {
                LocalOpenerEvidence::Withdrawn
            }
            _ => LocalOpenerEvidence::Unknown,
        };
        let visible_prefix = stamped && document.local_opener == LocalOpenerEvidence::VisiblePrefix;
        if !document.title_authoritative
            && (visible_prefix || local == LocalOpenerEvidence::Current || withdrawn.is_some())
            && self.has_pending_catalog_title(transaction)?
        {
            return Ok(TrajectoryTitleWrite::default());
        }
        let authoritative = document.title_authoritative
            || visible_prefix
            || (document.title_is_verified
                && document.derived_title.is_some()
                && document.local_opener == LocalOpenerEvidence::Unknown);
        let current = document.derived_title.filter(|_| {
            authoritative
                || local == LocalOpenerEvidence::Current
                || (!observed && local != LocalOpenerEvidence::Withdrawn)
        });
        let verified =
            current.is_some() && (authoritative || local == LocalOpenerEvidence::Current);
        Ok(TrajectoryTitleWrite {
            current,
            withdrawn,
            local,
            verified,
            authoritative,
        })
    }

    fn has_pending_catalog_title(&self, transaction: &Transaction<'_>) -> Result<bool> {
        let revision = self.document.source_revision;
        transaction
            .query_row(
                "SELECT EXISTS (
                 SELECT 1 FROM session_trajectories AS t
                 WHERE t.provider = 'codex' AND t.session_id = ?1 AND t.origin = 'native'
                   AND t.source_file_id = ?2 AND t.derived_title_is_local_opener IS NOT 1 AND (
                     EXISTS (SELECT 1 FROM codex_pending_catalog_titles p WHERE p.trajectory_id=t.id
                        AND p.source_file_id=t.source_file_id AND p.source_contents_id=t.source_contents_id
                        AND (p.evidence<>'uncataloged' OR t.source_contents_id=?3) AND p.title=t.derived_title)
                     OR (t.derived_title_is_verified AND EXISTS (
                         SELECT 1 FROM session_index i WHERE i.provider=t.provider AND i.session_id=t.session_id
                           AND i.source_file_id=t.source_file_id AND i.source_contents_id=t.source_contents_id
                           AND i.updated_at=t.source_updated_at AND omnis_compact_search_title(i.title)=t.derived_title))))",
                params![
                    self.session.id,
                    revision.file.map(Uuid::into_bytes),
                    revision.contents.map(Uuid::into_bytes)
                ],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    fn retain_document(&self, transaction: &Transaction<'_>, selected: bool) -> Result<()> {
        if selected || self.workspace.is_some() {
            // A matching document may already be current while discovery metadata still
            // describes an earlier copy. Never bind metadata to a rejected source.
            let matches =
                cached_trajectory_state(transaction, self.session)?.is_some_and(|state| {
                    state.source_updated_at == self.document.source_updated_at
                        && state.source_revision == self.document.source_revision
                        && state.document_version == self.document.document_version
                });
            if !matches {
                return Err(StoreError::TrajectorySourceChanged);
            }
        } else if self.document.origin == SessionTrajectoryOrigin::ImportedBundle {
            transaction
                .execute(
                    "UPDATE session_trajectories SET protected_by_bundle = 1
                         WHERE provider = ?1 AND session_id = ?2",
                    params![self.session.provider.to_string(), self.session.id],
                )
                .map_err(database_error)?;
        }
        Ok(())
    }

    fn bind_discovered_home(&self, transaction: &Transaction<'_>) -> Result<()> {
        transaction
            .prepare_cached(
                "UPDATE session_index SET codex_home=(SELECT home FROM current_codex_home)
                 WHERE provider='codex' AND session_id=?1 AND codex_home IS NULL
                 AND EXISTS (SELECT 1 FROM raw_session_catalog
                     WHERE provider='codex' AND session_id=?1 AND matches_unscoped_document)",
            )
            .map_err(database_error)?
            .execute([&self.session.id])
            .map_err(database_error)?;
        Ok(())
    }

    // A bounded read can verify a new name without replacing the full document already in search.
    fn refresh_current_title(
        &self,
        transaction: &Transaction<'_>,
        title: &TrajectoryTitleWrite<'_>,
    ) -> Result<()> {
        let document = &self.document;
        let revision = document.source_revision;
        transaction
            .execute(
                "UPDATE session_trajectories SET derived_title = ?1, derived_title_is_verified = ?2,
                     derived_title_is_local_opener = CASE WHEN ?2 THEN (?3 AND (?12 OR derived_title IS NOT ?1 OR derived_title_is_local_opener IS NOT 0)) END
                 WHERE provider = 'codex' AND session_id = ?4 AND origin = 'native'
                   AND document_version = ?5 AND source_updated_at = ?6
                   AND source_updated_submillis = ?13
                   AND source_file_id = ?7 AND source_contents_id IS ?8 AND missing_ancestor_id IS ?9
                   AND (?12 OR (?3 AND derived_title_is_local_opener IS NOT 0)
                        OR (?10 = derived_title AND (NOT derived_title_is_verified OR
                            (?11 AND derived_title_is_local_opener IS NOT 0))))
                   AND (derived_title IS NOT ?1 OR derived_title_is_verified <> ?2
                        OR derived_title_is_local_opener IS NOT (CASE WHEN ?2 THEN (?3 AND (?12 OR derived_title IS NOT ?1 OR derived_title_is_local_opener IS NOT 0)) END))",
                params![
                    title.current,
                    title.verified,
                    title.local == LocalOpenerEvidence::Current,
                    self.session.id,
                    document.document_version,
                    document.source_updated_at.timestamp_millis(),
                    revision.file.map(Uuid::into_bytes),
                    revision.contents.map(Uuid::into_bytes),
                    revision.missing_ancestor.map(Uuid::into_bytes),
                    title.withdrawn,
                    title.local == LocalOpenerEvidence::Withdrawn,
                    title.authoritative,
                    source_submillis(self.session.provider, document.source_updated_at),
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    fn record_workspace(&self, transaction: &Transaction<'_>) -> Result<()> {
        match self.workspace {
            Some(CodexWorkspaceUpdate::Verified(workspace)) => {
                workspace.record(transaction, self, false)
            }
            Some(CodexWorkspaceUpdate::Withdrawn(evidence)) => {
                if !self.source_complete
                    || self.document.origin != SessionTrajectoryOrigin::Native
                    || !matches!(self.guard, TrajectorySourceGuard::Observed(_))
                {
                    return Err(StoreError::TrajectorySourceChanged);
                }
                CodexWorkspace {
                    source_revision: *evidence,
                    project_path: None,
                    git_branch: None,
                }
                .record(transaction, self, true)
            }
            None => Ok(()),
        }
    }
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
        let state_root = path
            .parent()
            .filter(|root| !root.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        codex_legacy_home::migrate(&mut store.connection.borrow_mut(), state_root)?;
        initialize_catalog_home(&store.connection.borrow())?;
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
        self.bundle_ids_needing_imported_source(Some(0))
    }

    /// Returns bundle UUIDs with missing metadata. With a document version, also includes
    /// bundles whose search document is missing or older than that version.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored index cannot be read.
    pub fn bundle_ids_needing_imported_source(
        &self,
        document_version: Option<u32>,
    ) -> Result<Vec<Uuid>> {
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
                ) OR (?1 IS NOT NULL AND NOT EXISTS (
                    SELECT 1 FROM session_trajectories
                    WHERE provider = 'imported'
                      AND session_id = bundles.bundle_id AND document_version >= ?1
                ))
                ORDER BY bundles.bundle_id
                ",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([document_version], |row| row.get::<_, String>(0))
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

    /// Returns eligible imported session IDs whose search document is missing or outdated.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored index cannot be read.
    pub fn outdated_imported_documents(
        &self,
        eligible_ids: &[&str],
        document_version: u32,
    ) -> Result<Vec<String>> {
        let eligible = serde_json::to_string(eligible_ids)
            .map_err(|_| StoreError::Database(DatabaseFailure::Interface))?;
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT eligible.value FROM json_each(?1) AS eligible
                 WHERE NOT EXISTS (
                     SELECT 1 FROM session_trajectories
                     WHERE provider = 'imported' AND session_id = eligible.value
                       AND document_version >= ?2
                 )",
            )
            .map_err(database_error)?;
        statement
            .query_map(params![eligible, document_version], |row| row.get(0))
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns cached native-session metadata ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions(&self) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "
                SELECT provider, session_id, title, project_path, git_branch,
                       created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
                FROM scoped_session_catalog
                ORDER BY updated_at IS NULL, updated_at DESC, provider, session_id
                ",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], indexed_session_from_row)
            .map_err(database_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Reads picker entries for the selected native homes, including historical rows with no recorded home.
    ///
    /// # Errors
    ///
    /// Returns an error when the cached entries cannot be read.
    pub fn indexed_sessions_in_homes(
        &self,
        antigravity: Option<&Path>,
        desktop: Option<&Path>,
    ) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT catalog.provider, catalog.session_id, title, project_path, git_branch,
                created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
             FROM scoped_session_catalog AS catalog LEFT JOIN native_catalog_homes USING(provider,session_id)
             WHERE home IS NULL OR NOT verified OR home = CASE catalog.provider
                 WHEN 'antigravity' THEN ?1 WHEN 'antigravity-desktop' THEN ?2 END
             ORDER BY updated_at IS NULL, updated_at DESC, catalog.provider, catalog.session_id",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(
                params![
                    antigravity
                        .map(Path::as_os_str)
                        .map(std::ffi::OsStr::as_encoded_bytes),
                    desktop
                        .map(Path::as_os_str)
                        .map(std::ffi::OsStr::as_encoded_bytes)
                ],
                indexed_session_from_row,
            )
            .map_err(database_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Reads one provider's catalog for a selected home, including rows with no verified home.
    ///
    /// # Errors
    ///
    /// Returns an error when the cached entries cannot be read.
    pub fn indexed_sessions_for_provider_in_home(
        &self,
        provider: Provider,
        home: &Path,
    ) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT catalog.provider, catalog.session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
             FROM scoped_session_catalog AS catalog LEFT JOIN native_catalog_homes USING(provider,session_id)
             WHERE catalog.provider = ?1 AND CASE WHEN catalog.provider='antigravity-ide' THEN verified AND home=?2
                 ELSE home IS NULL OR NOT verified OR home=?2 END
             ORDER BY updated_at IS NULL, updated_at DESC, catalog.session_id",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(
                params![provider.to_string(), home.as_os_str().as_encoded_bytes()],
                indexed_session_from_row,
            )
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Reads cached metadata and title revisions from the same database snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the cached rows cannot be read.
    pub fn cached_sessions_with_titles(
        &self,
        provider: Option<Provider>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        self.read_trajectory_search(|snapshot| snapshot.cached_sessions_with_titles(provider))
    }

    /// Reads cached picker entries with names verified for the active native home.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the cached sessions.
    pub fn cached_picker_sessions_with_titles(
        &self,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        self.read_trajectory_search(|snapshot| {
            snapshot.read_cached_sessions_with_titles(None, true, None)
        })
    }

    /// Reads picker entries and their titles for the selected native homes in one snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the catalog or verify a title's home.
    pub fn cached_picker_sessions_with_titles_in_homes(
        &self,
        antigravity: Option<&Path>,
        desktop: Option<&Path>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        self.cached_picker_sessions_with_titles_for_native_homes(antigravity, desktop, None)
    }

    /// Reads cached picker entries for each selected native home.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog or its title evidence cannot be read.
    pub fn cached_picker_sessions_with_titles_for_native_homes(
        &self,
        antigravity: Option<&Path>,
        desktop: Option<&Path>,
        ide: Option<&Path>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        self.read_trajectory_search(|snapshot| {
            snapshot.read_cached_sessions_with_titles(None, true, Some([antigravity, desktop, ide]))
        })
    }

    /// Returns cached metadata for one provider, ordered by most recent update.
    ///
    /// # Errors
    ///
    /// Returns an error when cached metadata cannot be read.
    pub fn indexed_sessions_for_provider(&self, provider: Provider) -> Result<Vec<IndexedSession>> {
        let connection = self.connection.borrow();
        indexed_provider_rows(&connection, provider)
    }

    /// Reads raw catalog membership and source receipts for legacy names in one snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the active home's catalog.
    pub fn observe_codex_catalog_inventory(&self) -> Result<CodexCatalogInventory> {
        let connection = self.connection.borrow();
        let sql = format!(
            "SELECT provider,session_id,title,project_path,git_branch,created_at,updated_at,
             updated_at_approximate,event_count,source_file_id,source_contents_id,unscoped_name,coalesce(name_observable,0),
             CASE WHEN source_file_id IS NULL AND source_contents_id IS NULL THEN
               ({UNSTAMPED_CODEX_DOCUMENT} AND d.session_id=catalog.session_id) END
             FROM raw_session_catalog catalog WHERE provider='codex' AND (SELECT home FROM current_codex_home) IS NOT NULL
             ORDER BY updated_at IS NULL,updated_at DESC,session_id"
        );
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        let mut rows = statement.query([]).map_err(database_error)?;
        let mut inventory = CodexCatalogInventory::default();
        while let Some(row) = rows.next().map_err(database_error)? {
            let session = indexed_session_from_row(row).map_err(database_error)?;
            if row.get::<_, bool>(11).map_err(database_error)? {
                inventory
                    .unscoped
                    .insert(session.session.clone(), inventory.observed.len());
            }
            if row.get::<_, bool>(12).map_err(database_error)?
                && let Some(name) = &session.title
            {
                inventory
                    .observable_names
                    .insert(session.session.clone(), name.clone());
            }
            if let Some(receipt) = row.get::<_, Option<String>>(13).map_err(database_error)? {
                inventory
                    .unstamped_documents
                    .insert(session.session.clone(), receipt);
            }
            inventory.observed.push(session);
        }
        Ok(inventory)
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
        write_indexed_session(&self.connection.borrow(), session)
    }

    /// Returns whether one cached session has a metadata row.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or failed lookup.
    pub fn has_indexed_session(&self, session: &SessionRef) -> Result<bool> {
        validate_session_ref(session)?;
        self.connection.borrow().query_row(
            "SELECT EXISTS(SELECT 1 FROM session_index WHERE provider = ?1 AND session_id = ?2)",
            params![session.provider.to_string(), session.id],
            |row| row.get(0),
        ).map_err(database_error)
    }

    /// Removes cached Codex sessions whose recorded current rollout is unavailable.
    /// A newer catalog entry or a document from another source keeps its data.
    ///
    /// # Errors
    ///
    /// Returns an error for another provider or if the database cannot update the cache.
    pub fn remove_unavailable_codex_sessions(&self, observed: &[IndexedSession]) -> Result<()> {
        if observed.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        for prior in observed {
            if prior.session.provider != Provider::Codex {
                return Err(StoreError::InvalidSessionReference);
            }
            if cached_session(&transaction, &prior.session, false)?.as_ref() != Some(prior) {
                continue;
            }
            remove_codex_catalog_row(&transaction, prior, None)?;
        }
        transaction.commit().map_err(database_error)
    }

    /// Reads cached metadata for one selected session.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata cannot be read.
    pub fn indexed_session(&self, session: &SessionRef) -> Result<Option<IndexedSession>> {
        cached_session(&self.connection.borrow(), session, false)
    }

    /// Observes one selection before opening its source file.
    ///
    /// # Errors
    ///
    /// Returns an error when reading the index fails.
    pub fn observe_selected_session(
        &self,
        session: &SessionRef,
    ) -> Result<SelectedSessionObservation> {
        let mut connection = self.connection.borrow_mut();
        let transaction = connection.transaction().map_err(database_error)?;
        let state = cached_trajectory_state(&transaction, session)?;
        let (metadata, untrusted) = selected_catalog_rows(&transaction, session)?;
        let observation = SelectedSessionObservation {
            state,
            metadata: metadata.map(|row| row.metadata),
            untrusted,
            failure: cached_index_failure(&transaction, session)?,
            content_hash: cached_content_hash(&transaction, session)?,
        };
        transaction.commit().map_err(database_error)?;
        Ok(observation)
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
        if let Some(table) = desktop_declarations::table(session.provider) {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE session_id=?1"),
                    [&session.id],
                )
                .map_err(database_error)?;
        }
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
            "DELETE FROM ide_step_declarations WHERE ?1='antigravity-ide' AND session_id=?2",
            "DELETE FROM desktop_step_declarations WHERE ?1='antigravity-desktop' AND session_id=?2",
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
                params![index_check_key(&connection, provider)?],
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
                params![index_check_key(&connection, provider)?],
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
        let connection = self.connection.borrow();
        if let Some(key) = index_check_key(&connection, provider)? {
            record_session_check(&connection, &key, now_timestamp())?;
        }
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
        self.cache_discovered_sessions(provider, sessions, DiscoveryScope::CompleteProvider, None)
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
        self.cache_discovered_sessions(provider, sessions, DiscoveryScope::PartialProvider, None)
    }

    /// Stores a Codex listing and reconciles official names with the selected search documents.
    ///
    /// Capture `observed` before reading the provider. A partial name index can confirm a name
    /// but cannot establish that a previous name was removed.
    ///
    /// # Errors
    ///
    /// Returns an error if another reader changed the selected source or its name during discovery.
    pub fn refresh_codex_catalog(
        &self,
        sessions: &[IndexedSession],
        complete: bool,
        names_complete: bool,
        observed: &[IndexedSession],
    ) -> Result<()> {
        self.persist_codex_catalog(
            sessions,
            complete,
            CatalogNameObservations {
                observed,
                complete: names_complete,
                prior_assignments: &HashMap::new(),
                unstamped_documents: None,
            },
        )
    }

    /// Reconciles discovery with earlier name assignments from the active native sidecar.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored catalog changed during discovery.
    pub fn refresh_codex_catalog_with_name_history(
        &self,
        sessions: &[IndexedSession],
        complete: bool,
        names_complete: bool,
        inventory: &CodexCatalogInventory,
        prior_assignments: &HashMap<String, String>,
    ) -> Result<()> {
        self.persist_codex_catalog(
            sessions,
            complete,
            CatalogNameObservations {
                observed: &inventory.observed,
                complete: names_complete,
                prior_assignments,
                unstamped_documents: Some(&inventory.unstamped_documents),
            },
        )
    }

    fn persist_codex_catalog(
        &self,
        sessions: &[IndexedSession],
        complete: bool,
        names: CatalogNameObservations<'_>,
    ) -> Result<()> {
        self.write_indexed_catalog(
            Provider::Codex,
            sessions,
            if complete {
                DiscoveryScope::CompleteProvider
            } else {
                DiscoveryScope::PartialProvider
            },
            None,
            discovery::CatalogWrite::Unchecked,
            Some(names),
        )
        .map(|_| ())
    }

    /// Reconciles the Codex names observed during a search without marking a provider-wide refresh.
    ///
    /// # Errors
    ///
    /// Returns an error if another reader changed a selected source or name during discovery.
    pub fn merge_codex_search_catalog(
        &self,
        sessions: &[IndexedSession],
        names_complete: bool,
        observed: &[IndexedSession],
    ) -> Result<()> {
        self.merge_codex_search_catalog_with_name_history(
            sessions,
            names_complete,
            observed,
            &HashMap::new(),
        )
    }

    /// Reconciles search results with earlier assignments from the active native sidecar.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored catalog changed during discovery.
    pub fn merge_codex_search_catalog_with_name_history(
        &self,
        sessions: &[IndexedSession],
        names_complete: bool,
        observed: &[IndexedSession],
        prior_assignments: &HashMap<String, String>,
    ) -> Result<()> {
        self.write_indexed_catalog(
            Provider::Codex,
            sessions,
            DiscoveryScope::Workspace,
            None,
            discovery::CatalogWrite::Unchecked,
            Some(CatalogNameObservations {
                observed,
                complete: names_complete,
                prior_assignments,
                unstamped_documents: None,
            }),
        )
        .map(|_| ())
    }

    /// Merges the sessions observed in one workspace and preserves the last provider-wide check time.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched session providers or failed persistence.
    pub fn merge_workspace_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
    ) -> Result<()> {
        self.cache_discovered_sessions(provider, sessions, DiscoveryScope::Workspace, None)
    }

    /// Publishes observed catalog metadata and its native home in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid session metadata or a database failure.
    pub fn cache_discovered_sessions<'a>(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
        scope: impl Into<CatalogListing<'a>>,
        home: Option<&Path>,
    ) -> Result<()> {
        let listing = scope.into();
        if provider == Provider::Codex && listing.scope == DiscoveryScope::CompleteProvider {
            let observed = self.observe_codex_catalog_inventory()?;
            return self.refresh_codex_catalog_with_name_history(
                sessions,
                true,
                true,
                &observed,
                &HashMap::new(),
            );
        }
        self.write_discovered_sessions(
            provider,
            sessions,
            listing,
            home,
            discovery::CatalogWrite::Unchecked,
        )
        .map(|_| ())
    }

    /// Fills unavailable Desktop summaries from the same verified native home and session ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored summaries cannot be read.
    pub fn desktop_sessions_with_cached_summaries(
        &self,
        home: &Path,
        sessions: &[IndexedSession],
    ) -> Result<Vec<IndexedSession>> {
        retain_desktop_summaries(
            &self.connection.borrow(),
            home,
            sessions,
            &MissingDesktopSummaries::All,
        )
    }

    /// Returns the last step count verified for this Desktop home and database file.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid Desktop session reference or an unreadable catalog.
    pub fn verified_desktop_step_count(
        &self,
        session: &SessionRef,
        home: &Path,
        file: Uuid,
    ) -> Result<Option<usize>> {
        validate_session_ref(session)?;
        desktop_declarations::table(session.provider).ok_or(StoreError::InvalidSessionReference)?;
        desktop_declarations::verified(
            &self.connection.borrow(),
            &session.id,
            home.as_os_str().as_encoded_bytes(),
            file,
        )
    }

    /// Recovers a count when pre-upgrade readers recorded different declarations for one database.
    ///
    /// # Errors
    ///
    /// Returns an error if the cached declarations cannot be read or saved.
    pub fn reconcile_desktop_step_count(
        &self,
        session: &SessionRef,
        home: &Path,
        file: Uuid,
        matches: impl Fn(usize, Uuid) -> bool,
    ) -> Result<Option<usize>> {
        validate_session_ref(session)?;
        desktop_declarations::table(session.provider).ok_or(StoreError::InvalidSessionReference)?;
        desktop_declarations::reconcile(
            &self.connection.borrow(),
            session,
            home.as_os_str().as_encoded_bytes(),
            file,
            matches,
        )
    }

    fn write_discovered_sessions(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
        listing: CatalogListing<'_>,
        home: Option<&Path>,
        guard: discovery::CatalogWrite<'_>,
    ) -> Result<bool> {
        self.write_indexed_catalog(provider, sessions, listing, home, guard, None)
    }

    fn write_indexed_catalog<'a>(
        &self,
        provider: Provider,
        sessions: &[IndexedSession],
        scope: impl Into<CatalogListing<'a>>,
        home: Option<&Path>,
        guard: discovery::CatalogWrite<'_>,
        catalog: Option<CatalogNameObservations<'_>>,
    ) -> Result<bool> {
        let listing = scope.into();
        let scope = listing.scope;
        if scope == DiscoveryScope::Workspace
            && (matches!(guard, discovery::CatalogWrite::Publish(..))
                || sessions.is_empty() && matches!(guard, discovery::CatalogWrite::Unchecked))
        {
            return Ok(false);
        }
        let complete = scope == DiscoveryScope::CompleteProvider;
        let provider_wide = scope != DiscoveryScope::Workspace
            && !matches!(guard, discovery::CatalogWrite::Merge(_));
        validate_discovered_sessions(provider, sessions)?;
        if catalog.is_none()
            && complete
            && home.is_none()
            && provider_index_matches(self, provider, sessions)?
        {
            return record_unchanged_provider_refresh(self, provider).map(|()| true);
        }
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let (unverified, summaries_unavailable) = scope.desktop_observations(provider)?;
        let desktop_generation = guard.desktop_generation(&transaction, provider)?;
        if !guard.permits(&transaction, sessions, &unverified)? {
            return Ok(false);
        }
        let archived_ids = listing.archived_desktop_ids;
        discovery::remove_archived_ide_aliases(&transaction, provider, home, archived_ids)?;
        let alias_counts = listing.database_alias_counts;
        let adjusted;
        let mut rows_changed = true;
        let mut missing = Vec::new();
        let sessions = if let Some(names) = catalog {
            let changes = reconcile_codex_catalog(&transaction, sessions, names, complete)?;
            rows_changed = !changes.updated.is_empty() || !changes.missing.is_empty();
            missing = changes.missing;
            adjusted = changes.updated;
            adjusted.as_slice()
        } else if !matches!(summaries_unavailable, MissingDesktopSummaries::None) {
            adjusted = retain_desktop_summaries(
                &transaction,
                home.ok_or(StoreError::InvalidSessionReference)?,
                sessions,
                &summaries_unavailable,
            )?;
            adjusted.as_slice()
        } else {
            sessions
        };
        let provider_name = provider.to_string();
        if complete && rows_changed && catalog.is_none() {
            transaction
                .execute(
                    "DELETE FROM session_index WHERE provider = ?1",
                    params![provider_name],
                )
                .map_err(database_error)?;
        }
        let indexed_at = now_timestamp();
        if rows_changed {
            let mut statement =
                prepare_indexed_session(&transaction, !complete || catalog.is_some())?;
            for session in sessions {
                write_indexed_session_row(&mut statement, &provider_name, session, indexed_at)?;
                if let Some(home) = home {
                    record_discovery_home(&transaction, &session.session, home)?;
                    desktop_declarations::record_listing(
                        &transaction,
                        session,
                        home,
                        &summaries_unavailable,
                        desktop_generation,
                        alias_counts.and_then(|counts| counts(&session.session.id)),
                    )?;
                }
            }
        }
        if let (Some(home), Some(file_at)) = (home, listing.file_at) {
            qualify_legacy_document_homes(&transaction, home, sessions, file_at)?;
        }
        if complete {
            prune_stale_native_trajectories(&transaction, &provider_name)?;
            for prior in missing {
                let receipt =
                    catalog.and_then(|names| names.unstamped_documents?.get(&prior.session));
                remove_codex_catalog_row(&transaction, &prior, receipt.map(String::as_str))?;
            }
        }
        if catalog.is_some() && (provider_wide || rows_changed) {
            reconcile_named_trajectories(&transaction, (!provider_wide).then_some(sessions))?;
        }
        if provider_wide && let Some(key) = index_check_key(&transaction, provider)? {
            if complete {
                record_complete_catalog(&transaction, &key, indexed_at, rows_changed)?;
            }
            record_session_check(&transaction, &key, indexed_at)?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(true)
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
                redacted_text,
                source_revision: SourceRevision::default(),
                source_updated_at,
                source_byte_count,
                indexed_byte_count,
                truncation_strategy,
                source_complete,
                provider_limited: false,
                origin,
                document_version: 0,
                derived_title: None,
                title_is_verified: false,
                local_opener: LocalOpenerEvidence::Unknown,
                database_step_proof: None,
                desktop_projection: None,
                legacy_utf8_text: None,
                withdrawn_title: None,
                title_authoritative: false,
            },
        )
    }

    /// Stores one versioned search document.
    ///
    /// A write must preserve the version and coverage of the indexed source. To record a
    /// replacement native source, first observe the document and use the guarded batch writer.
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
            TrajectoryWriteContext::Discovered(&[]),
            1,
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
            TrajectoryWriteContext::Discovered(&[]),
            MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
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

    /// Verifies the selected native sources inside the write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if a source changed or the batch cannot be stored.
    pub fn upsert_observed_trajectory_documents_checked(
        &self,
        documents: &[(&SessionRef, TrajectoryDocument<'_>, TrajectorySourceGuard)],
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_trajectory_documents_with_limits(
            documents
                .iter()
                .map(|&(session, document, guard)| (session, document, guard, None)),
            TrajectoryWriteContext::Discovered(&[]),
            MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
            if documents.len() == 1 {
                usize::MAX
            } else {
                MAX_TRAJECTORY_WRITE_BATCH_BYTES
            },
            verify,
        )
    }

    /// Stores each document and applies any workspace update from the same read.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid batch, a conflicting source, or a failed write.
    pub fn upsert_observed_trajectory_documents_with_workspaces(
        &self,
        documents: &[(
            &SessionRef,
            TrajectoryDocument<'_>,
            TrajectorySourceGuard,
            Option<&CodexWorkspaceUpdate>,
        )],
    ) -> Result<()> {
        self.upsert_discovered_trajectory_documents(documents, &[])
    }

    /// Publishes a background batch while its observed Codex names still match the catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if a name changed during indexing, or if a document cannot be stored.
    pub fn upsert_discovered_trajectory_documents(
        &self,
        documents: &[(
            &SessionRef,
            TrajectoryDocument<'_>,
            TrajectorySourceGuard,
            Option<&CodexWorkspaceUpdate>,
        )],
        observed: &[&IndexedSession],
    ) -> Result<()> {
        self.upsert_discovered_trajectory_documents_checked(documents, observed, |_, _| true)
            .map(|_| ())
    }

    /// Publishes a background batch after checking its sources and observed Codex names. Returns the number of documents rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error if a name changed during indexing, or if a document cannot be stored.
    pub fn upsert_discovered_trajectory_documents_checked(
        &self,
        documents: &[(
            &SessionRef,
            TrajectoryDocument<'_>,
            TrajectorySourceGuard,
            Option<&CodexWorkspaceUpdate>,
        )],
        observed: &[&IndexedSession],
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_trajectory_documents_with_limits(
            documents.iter().copied(),
            TrajectoryWriteContext::Discovered(observed),
            MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
            if documents.len() == 1 {
                usize::MAX
            } else {
                MAX_TRAJECTORY_WRITE_BATCH_BYTES
            },
            verify,
        )
    }

    /// Stores an Antigravity locator together with the search document for its verified native source.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is unverified, the selected session changed, or a write fails.
    pub fn upsert_verified_session_document_checked(
        &self,
        metadata: &IndexedSession,
        document: TrajectoryDocument<'_>,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_verified_session_document_in_home(metadata, None, document, guard, verify)
    }

    /// Publishes a verified native document together with its catalog metadata and observed home.
    ///
    /// # Errors
    ///
    /// Returns an error if the source changed or the transaction failed.
    pub fn upsert_verified_session_document_in_home(
        &self,
        metadata: &IndexedSession,
        home: Option<&Path>,
        document: TrajectoryDocument<'_>,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        self.upsert_verified_session_document_with_catalog(
            metadata, home, false, document, guard, verify,
        )
    }

    /// Publishes a verified native document and retains known Desktop metadata when its summary is unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error if the source changed or the transaction failed.
    pub fn upsert_verified_session_document_with_catalog(
        &self,
        metadata: &IndexedSession,
        home: Option<&Path>,
        desktop_summaries_unavailable: bool,
        document: TrajectoryDocument<'_>,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        if !TrajectorySourceGuard::uses_catalog_home(metadata.session.provider)
            || (desktop_summaries_unavailable
                && (desktop_declarations::table(metadata.session.provider).is_none()
                    || home.is_none()))
            || document.origin == SessionTrajectoryOrigin::ImportedBundle
            || document.source_revision.file.is_none()
            || document.source_revision.contents.is_none()
            || !matches!(guard, TrajectorySourceGuard::ObservedDocument(_))
        {
            return Err(StoreError::InvalidSessionReference);
        }
        let guard = guard.with_catalog_step_count(
            metadata.session.provider,
            metadata.event_count,
            desktop_summaries_unavailable,
        );
        self.upsert_trajectory_documents_with_limits(
            std::iter::once((&metadata.session, document, guard, None)),
            TrajectoryWriteContext::Locator {
                row: metadata,
                home,
                desktop_summaries_unavailable,
            },
            1,
            usize::MAX,
            verify,
        )
    }

    /// Stores an imported session's locator and search document in the same transaction.
    /// Returns whether the search document was rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid imported metadata, an invalid document, or a failed write.
    pub fn upsert_imported_session_document(
        &self,
        locator: &IndexedSession,
        document: &TrajectoryDocument<'_>,
    ) -> Result<bool> {
        if locator.session.provider != Provider::Imported
            || document.origin != SessionTrajectoryOrigin::ImportedBundle
        {
            return Err(StoreError::InvalidSessionReference);
        }
        self.upsert_trajectory_documents_with_limits(
            std::iter::once((
                &locator.session,
                *document,
                TrajectorySourceGuard::NewerSource,
                None,
            )),
            TrajectoryWriteContext::Locator {
                row: locator,
                home: None,
                desktop_summaries_unavailable: false,
            },
            1,
            usize::MAX,
            |_, _| true,
        )
        .map(|rewritten| rewritten != 0)
    }

    /// Saves metadata and search text for the same selected native source in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if another reader changed the observed source or metadata, or the write fails.
    pub fn upsert_selected_session_document(
        &self,
        selected: &IndexedSession,
        observation: &SelectedSessionObservation,
        document: TrajectoryDocument<'_>,
    ) -> Result<()> {
        self.upsert_selected_session_document_with_workspace(selected, observation, document, None)
    }

    /// Saves the selected metadata, search text, and workspace update together.
    ///
    /// # Errors
    ///
    /// Returns an error if the source changed or persistence failed.
    pub fn upsert_selected_session_document_with_workspace(
        &self,
        selected: &IndexedSession,
        observation: &SelectedSessionObservation,
        document: TrajectoryDocument<'_>,
        workspace: Option<&CodexWorkspaceUpdate>,
    ) -> Result<()> {
        self.upsert_selected_session_document_with_name_coverage(
            selected,
            observation,
            document,
            workspace,
            true,
        )
    }

    /// Saves a selected read with the coverage of its provider name lookup.
    ///
    /// # Errors
    ///
    /// Returns an error if the source or catalog changed during the read.
    pub fn upsert_selected_session_document_with_name_coverage(
        &self,
        selected: &IndexedSession,
        observation: &SelectedSessionObservation,
        document: TrajectoryDocument<'_>,
        workspace: Option<&CodexWorkspaceUpdate>,
        name_observed: bool,
    ) -> Result<()> {
        self.upsert_selected_session_document_with_name_history(
            selected,
            observation,
            document,
            workspace,
            name_observed,
            None,
        )
    }

    /// Saves a selected read and checks an earlier native assignment against the legacy catalog receipt.
    ///
    /// # Errors
    ///
    /// Returns an error if the source or catalog changed during the read.
    pub fn upsert_selected_session_document_with_name_history(
        &self,
        selected: &IndexedSession,
        observation: &SelectedSessionObservation,
        document: TrajectoryDocument<'_>,
        workspace: Option<&CodexWorkspaceUpdate>,
        name_observed: bool,
        prior_assignment: Option<&str>,
    ) -> Result<()> {
        self.upsert_selected_session_document_with_name_history_counted(
            selected,
            observation,
            document,
            workspace,
            name_observed,
            prior_assignment,
        )
        .map(|_| ())
    }

    /// Saves a selected read with its name history and returns the number of documents rewritten.
    ///
    /// # Errors
    ///
    /// Returns an error if the source or catalog changed during the read.
    pub fn upsert_selected_session_document_with_name_history_counted(
        &self,
        selected: &IndexedSession,
        observation: &SelectedSessionObservation,
        document: TrajectoryDocument<'_>,
        workspace: Option<&CodexWorkspaceUpdate>,
        name_observed: bool,
        prior_assignment: Option<&str>,
    ) -> Result<usize> {
        if !selected
            .source_revision
            .is_some_and(|revision| revision.matches_file_contents(document.source_revision))
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let mut selected = Cow::Borrowed(selected);
        if matches!(workspace, Some(CodexWorkspaceUpdate::Withdrawn(_))) {
            let row = selected.to_mut();
            row.project_path = None;
            row.git_branch = None;
        }
        let title;
        let document = if !name_observed
            && document.origin == SessionTrajectoryOrigin::Native
            && let Some(name) = observation.unindexed_codex_name(&selected).or_else(|| {
                observation.corroborated_legacy_codex_name(&selected, prior_assignment?)
            }) {
            selected.to_mut().title = Some(name.to_owned());
            title = omnis_core::compact_search_title(name);
            TrajectoryDocument {
                derived_title: title.as_deref(),
                title_authoritative: true,
                title_is_verified: title.is_some(),
                local_opener: LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                ..document
            }
        } else {
            document
        };
        self.upsert_trajectory_documents_with_limits(
            std::iter::once((
                &selected.session,
                document,
                TrajectorySourceGuard::Observed(observation.state),
                workspace,
            )),
            TrajectoryWriteContext::Selected(SelectedMetadata {
                selected: &selected,
                observation,
                name_observed,
            }),
            1,
            usize::MAX,
            |_, _| true,
        )
    }

    /// Reconnects current discovery metadata to an unchanged indexed source after a failed read of another copy.
    ///
    /// # Errors
    ///
    /// Returns an error if the document, metadata, or failure changed after observation.
    pub fn restore_indexed_source(
        &self,
        selected: &IndexedSession,
        expected: TrajectoryIndexState,
        observed_metadata: Option<&IndexedSession>,
        failure: Option<TrajectoryIndexFailure>,
    ) -> Result<()> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        if !selected
            .source_revision
            .is_some_and(|revision| revision.matches_file_contents(expected.source_revision))
            || cached_trajectory_state(&transaction, &selected.session)? != Some(expected)
            || cached_session(&transaction, &selected.session, true)?.as_ref() != observed_metadata
            || cached_index_failure(&transaction, &selected.session)? != failure
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        write_indexed_session(&transaction, selected)?;
        if let Some(failure) = failure {
            clear_observed_failure(&transaction, &selected.session, failure)?;
        }
        transaction.commit().map_err(database_error)
    }

    fn upsert_trajectory_documents_with_limits<'s, 'd>(
        &self,
        documents: impl ExactSizeIterator<
            Item = (
                &'s SessionRef,
                TrajectoryDocument<'d>,
                TrajectorySourceGuard,
                Option<&'d CodexWorkspaceUpdate>,
            ),
        >,
        context: TrajectoryWriteContext<'_>,
        max_documents: usize,
        max_indexed_bytes: usize,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        if documents.len() == 0 {
            return Ok(0);
        }
        if documents.len() > max_documents {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }

        let mut total_indexed_bytes = 0_usize;
        let mut prepared = Vec::with_capacity(documents.len());
        for (session, document, guard, workspace) in documents {
            validate_trajectory_document(session, &document, guard)?;
            total_indexed_bytes = total_indexed_bytes
                .checked_add(document.indexed_byte_count)
                .filter(|total| *total <= max_indexed_bytes)
                .ok_or(StoreError::TrajectoryBatchTooLarge)?;

            // Provider-reported omissions keep `complete` false through the strategy, but a full
            // read of such a source still covers it; clearing `source_complete` would re-index it
            // forever.
            let source_complete =
                document.source_complete && document.truncation_strategy != "legacy_unknown";
            let complete = source_complete
                && document.truncation_strategy == "none"
                && document.source_byte_count == document.indexed_byte_count;
            let source_byte_count = i64::try_from(document.source_byte_count)
                .map_err(|_| StoreError::InvalidSessionReference)?;
            let indexed_byte_count = i64::try_from(document.indexed_byte_count)
                .map_err(|_| StoreError::InvalidSessionReference)?;
            prepared.push(PreparedTrajectoryWrite {
                workspace,
                session,
                content_hash: Sha256::digest(document.redacted_text.as_bytes()).to_vec(),
                guard,
                document,
                source_complete,
                complete,
                source_byte_count,
                indexed_byte_count,
            });
        }

        self.persist_trajectory_documents(&prepared, context, verify)
    }

    fn persist_trajectory_documents(
        &self,
        prepared: &[PreparedTrajectoryWrite<'_, '_>],
        context: TrajectoryWriteContext<'_>,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let unowned = check_discovered_catalog(&transaction, context)?;
        let selected_metadata = match context {
            TrajectoryWriteContext::Selected(metadata) => Some(metadata),
            _ => None,
        };
        let mut scoped_documents = 0;
        let mut rewritten: Vec<(i64, &str)> = Vec::with_capacity(prepared.len());
        let mut update_catalog = false;
        let mut upsert_parent = transaction
            .prepare_cached(UPSERT_TRAJECTORY_PARENT_SQL)
            .map_err(database_error)?;
        for write in prepared {
            if let Some(metadata) = selected_metadata {
                update_catalog = metadata.needs_catalog_update(
                    &transaction,
                    write.session,
                    &write.content_hash,
                )?;
            }
            let document = &write.document;
            write
                .guard
                .check(&transaction, write.session, Some(document))?;
            let (verified_partial, retain_text) =
                match scoped_native_documents::write(&transaction, write, context, &rewritten)? {
                    scoped_native_documents::WriteResult::Handled(changed) => {
                        scoped_documents += changed;
                        continue;
                    }
                    scoped_native_documents::WriteResult::Primary {
                        verified_partial,
                        retain_text,
                    } => (verified_partial, retain_text),
                };
            codex_title_recheck::accept(&transaction, write, selected_metadata)?;
            let observed = matches!(write.guard, TrajectorySourceGuard::Observed(_));
            let native_codex = write.session.provider == Provider::Codex
                && document.origin == SessionTrajectoryOrigin::Native;
            if let Some(metadata) = selected_metadata
                && native_codex
            {
                metadata.reconcile_name(&transaction)?;
            }
            let title = write.title_evidence(&transaction, observed)?;
            let reuse_chunks = write.reuses_revision_chunks(&transaction)?;
            let trajectory_id = write.upsert_parent(
                &mut upsert_parent,
                &title,
                selected_metadata.is_some(),
                verified_partial,
            )?;
            if !context.publish_locator(&transaction, write, trajectory_id.is_some())? {
                continue;
            }
            if trajectory_id.is_none()
                && native_codex
                && observed
                && (title.authoritative
                    || title.local == LocalOpenerEvidence::Current
                    || title.withdrawn.is_some())
            {
                write.refresh_current_title(&transaction, &title)?;
            }
            if let Some(trajectory_id) = trajectory_id {
                if native_codex && observed && unowned.contains(write.session) {
                    write.bind_discovered_home(&transaction)?;
                }
                if !reuse_chunks {
                    // Only the last changed text for a session needs new chunks.
                    rewritten.retain(|(other, _)| *other != trajectory_id);
                    rewritten.push((trajectory_id, document.redacted_text));
                }
            } else {
                write.retain_document(&transaction, selected_metadata.is_some())?;
            }
            if native_codex {
                write.carry_pending_catalog_title(&transaction)?;
            }
            codex_title_recheck::retire(&transaction, write)?;
            write.record_workspace(&transaction)?;
            record_verified_document_home(&transaction, write, context)?;
            primary_legacy::retain(&transaction, trajectory_id, retain_text.as_deref())?;
            clear_observed_native_failure(&transaction, write)?;
        }
        drop(upsert_parent);
        replace_trajectory_chunks(&transaction, &rewritten)?;
        scoped_native_documents::chunks::flush(&transaction)?;
        if let Some(metadata) = selected_metadata {
            metadata.publish(&transaction, update_catalog)?;
        }
        for write in prepared {
            let revision = write.document.source_revision;
            if revision.file.is_some() && !verify(write.session, revision) {
                return Err(StoreError::TrajectorySourceChanged);
            }
        }
        transaction.commit().map_err(database_error)?;
        Ok(rewritten.len() + scoped_documents)
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
                    session.provider != Provider::Imported,
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
        self.source_read_covers(session, source_updated_at, None, document_version)
    }

    /// Checks whether a full source read covered the selected provider revision.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database read.
    pub fn session_trajectory_source_revision_is_current(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
        source_revision: SourceRevision,
    ) -> Result<bool> {
        self.source_read_covers(session, source_updated_at, Some(source_revision), 0)
    }

    fn source_read_covers(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
        source_revision: Option<SourceRevision>,
        document_version: u32,
    ) -> Result<bool> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow();
        let revision = source_revision.unwrap_or_default();
        connection
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM session_trajectories
                    WHERE provider = ?1 AND session_id = ?2
                      AND source_complete = 1 AND source_updated_at >= ?3
                      AND (NOT ?4 OR (source_file_id IS ?5 AND missing_ancestor_id IS ?6 AND source_contents_id IS ?8))
                      AND (NOT ?7 OR (source_updated_at = ?3 AND source_updated_submillis = ?10)) AND document_version >= ?9
                )",
                params![
                    session.provider.to_string(),
                    session.id,
                    source_updated_at.timestamp_millis(),
                    source_revision.is_some(),
                    revision.file.map(Uuid::into_bytes),
                    revision.missing_ancestor.map(Uuid::into_bytes),
                    session.provider != Provider::Imported,
                    revision.contents.map(Uuid::into_bytes),
                    document_version,
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
        cached_trajectory_state(&self.connection.borrow(), session)
    }

    /// Reads the SHA-256 digest of a full Codex projection that predates file and contents revisions.
    /// Version 5 rows must match the digest and source time recorded during the schema upgrade.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database read.
    pub fn legacy_codex_projection_hash(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
    ) -> Result<Option<[u8; 32]>> {
        validate_session_ref(session)?;
        if session.provider != Provider::Codex {
            return Ok(None);
        }
        self.connection
            .borrow()
            .query_row(
                "SELECT t.content_hash FROM session_trajectories AS t
                 WHERE t.provider = 'codex' AND t.session_id = ?1 AND t.source_updated_at = ?2
                   AND t.origin = 'native' AND t.source_complete = 1
                   AND t.source_file_id IS NULL AND t.source_contents_id IS NULL AND t.missing_ancestor_id IS NULL
                   AND typeof(t.content_hash) = 'blob' AND length(t.content_hash) = 32
                   AND (t.document_version = 4 OR (t.document_version = 6 AND t.source_updated_submillis = ?3)
                        OR (t.document_version = 5 AND EXISTS (
                       SELECT 1 FROM legacy_codex_v5_projections AS old
                       WHERE old.trajectory_id = t.id AND old.source_updated_at = t.source_updated_at
                         AND old.content_hash = t.content_hash)))",
                params![session.id, source_updated_at.timestamp_millis(), source_submillis(session.provider, source_updated_at)],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)
    }

    /// Observes the selected search document before a native read whose event time can regress.
    ///
    /// # Errors
    ///
    /// Returns an error when the reference is invalid or the document cannot be read.
    pub fn observe_trajectory_document(
        &self,
        session: &SessionRef,
    ) -> Result<TrajectorySourceGuard> {
        self.observe_document_before_read(session, false)
    }

    /// Observes the verified Antigravity catalog locator before reading a background search document.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot read the document, failure, or catalog state.
    pub fn observe_discovered_trajectory_document(
        &self,
        session: &SessionRef,
    ) -> Result<TrajectorySourceGuard> {
        self.observe_document_before_read(session, true)
    }

    fn observe_document_before_read(
        &self,
        session: &SessionRef,
        check_catalog: bool,
    ) -> Result<TrajectorySourceGuard> {
        validate_session_ref(session)?;
        let connection = self.connection.borrow();
        let observation = if TrajectorySourceGuard::uses_catalog_home(session.provider) {
            let transaction = connection.unchecked_transaction().map_err(database_error)?;
            let snapshot = TrajectoryDocumentObservation {
                catalog: if check_catalog {
                    CatalogObservation::read(&transaction, session)?
                } else {
                    CatalogObservation::Unchecked
                },
                document: document_receipt(&transaction, session)?,
                failure: native_failure_receipt(&transaction, session)?,
                desktop_steps: None,
                database_contents: None,
                desktop_generation: desktop_declarations::generation(
                    &transaction,
                    session.provider,
                )?,
                incomplete_native_history: false,
            };
            transaction.commit().map_err(database_error)?;
            snapshot
        } else {
            TrajectoryDocumentObservation {
                catalog: CatalogObservation::Unchecked,
                document: document_receipt(&connection, session)?,
                failure: None,
                desktop_steps: None,
                database_contents: None,
                desktop_generation: None,
                incomplete_native_history: false,
            }
        };
        Ok(TrajectorySourceGuard::ObservedDocument(observation))
    }

    /// Checks whether a previously scanned archive covers this exact selected native source.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be read.
    pub fn current_limited_native_archive(
        &self,
        session: &SessionRef,
        home: &Path,
        revision: SourceRevision,
        version: u32,
    ) -> Result<bool> {
        let (Some(file), Some(contents)) = (revision.file, revision.contents) else {
            return Ok(false);
        };
        self.connection
            .borrow()
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM scoped_native_documents d
             JOIN native_catalog_homes h ON h.provider=d.provider AND h.session_id=d.session_id
             WHERE d.provider=?1 AND d.session_id=?2 AND d.source_home=?3 AND d.source_file=?4
               AND d.source_contents=?5 AND d.document_version=?6 AND d.provider_limited
               AND h.verified AND h.home=d.source_home)",
            )
            .and_then(|mut statement| {
                statement.query_row(
                    params![
                        session.provider.to_string(),
                        session.id,
                        home.as_os_str().as_encoded_bytes(),
                        file.as_bytes(),
                        contents.as_bytes(),
                        version
                    ],
                    |row| row.get(0),
                )
            })
            .map_err(database_error)
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
                "SELECT provider, session_id, source_updated_at, source_complete, document_version, source_updated_submillis, provider_limited, native_summary, source_file_id, missing_ancestor_id, source_contents_id
                 FROM session_trajectories",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, bool>(7)?,
                    source_revision_from_row(row, 8)?,
                ))
            })
            .map_err(database_error)?;
        let mut states = std::collections::HashMap::new();
        for row in rows {
            let (
                provider,
                session_id,
                source_updated_at,
                source_complete,
                document_version,
                remainder,
                provider_limited,
                native_summary,
                source_revision,
            ) = row.map_err(|_| StoreError::CorruptStore)?;
            let provider = provider
                .parse::<Provider>()
                .map_err(|_| StoreError::CorruptStore)?;
            states.insert(
                SessionRef::new(provider, session_id),
                TrajectoryIndexState {
                    source_updated_at: source_time_from_db(source_updated_at, remainder)?,
                    source_time_precise: remainder.is_some(),
                    source_complete,
                    provider_limited,
                    native_summary,
                    source_revision,
                    document_version: u32::try_from(document_version)
                        .map_err(|_| StoreError::CorruptStore)?,
                },
            );
        }
        Ok(states)
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
                        native_summary, source_file_id, source_contents_id
                 FROM trajectory_index_failures",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    trajectory_failure_from_row(row, 2)?,
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

    /// Reads the most recent failed attempt for one session.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session or a failed database read.
    pub fn trajectory_index_failure(
        &self,
        session: &SessionRef,
    ) -> Result<Option<TrajectoryIndexFailure>> {
        validate_session_ref(session)?;
        cached_index_failure(&self.connection.borrow(), session)
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

    /// Records a failed read if the indexed document still matches the caller's observation.
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

    /// Verifies the selected provider file before committing the failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the indexed document or selected file changed, or the write fails.
    pub fn record_observed_trajectory_index_failure_checked(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
        guard: TrajectorySourceGuard,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<()> {
        validate_session_ref(session)?;
        if failure.native_summary && session.provider != Provider::Antigravity {
            return Err(StoreError::InvalidSessionReference);
        }
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        guard.check(&transaction, session, None)?;
        if TrajectorySourceGuard::uses_catalog_home(session.provider)
            && let TrajectorySourceGuard::ObservedDocument(observed) = guard
            && native_failure_receipt(&transaction, session)? != observed.failure
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        transaction
            .execute(
                "INSERT INTO trajectory_index_failures (
                     provider, session_id, source_updated_at, document_version, failed_at, source_updated_submillis,
                     native_summary, source_file_id, source_contents_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (provider, session_id) DO UPDATE SET
                     source_updated_at = excluded.source_updated_at,
                     source_updated_submillis = excluded.source_updated_submillis,
                     native_summary = excluded.native_summary,
                     source_file_id = excluded.source_file_id,
                     source_contents_id = excluded.source_contents_id,
                     document_version = excluded.document_version,
                     failed_at = excluded.failed_at
                 WHERE excluded.document_version >= trajectory_index_failures.document_version
                   AND (trajectory_index_failures.source_file_id IS NULL OR excluded.source_file_id IS NOT NULL)",
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
                    failure.native_summary,
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

    /// Clears a failed indexing attempt if it still matches the failure the caller saw.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database write.
    pub fn clear_observed_trajectory_index_failure(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
    ) -> Result<()> {
        validate_session_ref(session)?;
        clear_observed_failure(&self.connection.borrow(), session, *failure)
    }

    /// Allows the indexer to retry a failed source while its revision still hides the old text.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database write.
    pub fn retry_observed_trajectory_index_failure(
        &self,
        session: &SessionRef,
        failure: &TrajectoryIndexFailure,
    ) -> Result<()> {
        validate_session_ref(session)?;
        update_observed_failure(
            &self.connection.borrow(),
            session,
            *failure,
            "UPDATE trajectory_index_failures SET document_version = 0",
        )
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

    /// Returns the derived title that survived a search document update.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed read.
    pub fn trajectory_title(&self, session: &SessionRef) -> Result<Option<String>> {
        TrajectorySearchSnapshot {
            connection: &self.connection.borrow(),
        }
        .trajectory_title(session)
    }

    /// Reads the indexed title for a session discovered in this native home.
    ///
    /// # Errors
    ///
    /// Returns an error when the title or its source identity cannot be read.
    pub fn trajectory_title_for_native_source(
        &self,
        session: &SessionRef,
        home: &Path,
        file: Option<Uuid>,
    ) -> Result<Option<String>> {
        self.read_trajectory_search(|snapshot| snapshot.native_source_title(session, home, file))
    }

    /// Lists exact rollout revisions whose verified opener is waiting for a readable name index.
    ///
    /// # Errors
    ///
    /// Returns an error when the pending revisions cannot be read.
    pub fn legacy_codex_name_retries(&self) -> Result<HashMap<SessionRef, SourceRevision>> {
        codex_title_recheck::name_retries(&self.connection.borrow())
    }

    /// Returns a previously protected title eligible for one ordinary-rollout verification.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed cache read.
    pub fn legacy_codex_opener_candidate(
        &self,
        session: &SessionRef,
        source_file: Uuid,
    ) -> Result<Option<LegacyCodexOpenerCandidate>> {
        validate_session_ref(session)?;
        codex_title_recheck::candidate(&self.connection.borrow(), session, source_file)
    }

    /// Returns a provisional title that a rollback in this file could withdraw. A verified ordinary
    /// rollback can also withdraw a known local opener or an unclassified legacy title.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed read.
    pub fn withdrawable_trajectory_title(
        &self,
        session: &SessionRef,
        file: Uuid,
        include_verified_local: bool,
    ) -> Result<Option<String>> {
        validate_session_ref(session)?;
        if session.provider != Provider::Codex {
            return Ok(None);
        }
        self.connection.borrow().query_row(
            "SELECT derived_title FROM session_trajectories
             WHERE provider = 'codex' AND session_id = ?1 AND source_file_id = ?2
               AND origin = 'native' AND length(derived_title) > 0
               AND (NOT derived_title_is_verified OR (?3 AND derived_title_is_local_opener IS NOT 0))",
            params![session.id, file.into_bytes(), include_verified_local], |row| row.get(0),
        ).optional().map_err(database_error)
    }

    /// Returns native Codex documents whose catalog row belongs to another rollout revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the index cannot be read.
    pub fn conflicting_codex_catalog_rows(&self) -> Result<std::collections::HashSet<SessionRef>> {
        TrajectorySearchSnapshot {
            connection: &self.connection.borrow(),
        }
        .conflicting_codex_catalog_rows()
    }

    /// Returns the stored title or the source of a failure that hides it.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reference or if the store cannot be read.
    pub fn indexed_trajectory_title(
        &self,
        session: &SessionRef,
    ) -> Result<Option<IndexedTrajectoryTitle>> {
        TrajectorySearchSnapshot {
            connection: &self.connection.borrow(),
        }
        .indexed_trajectory_title(session)
    }

    /// Returns titles and source revisions from the same read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read.
    pub fn trajectory_titles_with_sources(&self) -> Result<Vec<IndexedTrajectoryTitle>> {
        TrajectorySearchSnapshot {
            connection: &self.connection.borrow(),
        }
        .trajectory_titles_with_sources()
    }

    /// Returns redacted titles derived from indexed conversations.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read or contains invalid data.
    pub fn trajectory_titles(&self) -> Result<std::collections::HashMap<SessionRef, String>> {
        TrajectorySearchSnapshot {
            connection: &self.connection.borrow(),
        }
        .trajectory_titles()
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
        self.read_trajectory_search(|snapshot| {
            snapshot.search_session_trajectory_page_with_eligibility(query, limit, None, None, None)
        })
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
        self.read_trajectory_search(|snapshot| {
            snapshot.search_session_trajectory_page_for_sessions(query, limit, eligible_sessions)
        })
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
        self.read_trajectory_search(|snapshot| {
            snapshot.search_selected_trajectory_page(query, limit, eligible_sessions)
        })
    }

    /// Runs a search against a consistent view of document ownership, titles and full-text matches.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot open or the query fails.
    pub fn read_trajectory_search<T, E>(
        &self,
        read: impl FnOnce(&TrajectorySearchSnapshot<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StoreError>,
    {
        let connection = self.connection.borrow();
        let transaction = connection.unchecked_transaction().map_err(database_error)?;
        read(&TrajectorySearchSnapshot {
            connection: &transaction,
        })
    }
}

#[derive(Clone, Copy)]
enum TrajectoryWriteContext<'a> {
    Discovered(&'a [&'a IndexedSession]),
    Selected(SelectedMetadata<'a>),
    Locator {
        row: &'a IndexedSession,
        home: Option<&'a Path>,
        desktop_summaries_unavailable: bool,
    },
}

impl TrajectoryWriteContext<'_> {
    fn publish_locator(
        self,
        transaction: &Transaction<'_>,
        write: &PreparedTrajectoryWrite<'_, '_>,
        accepted: bool,
    ) -> Result<bool> {
        let Self::Locator {
            row,
            home,
            desktop_summaries_unavailable,
        } = self
        else {
            return Ok(true);
        };
        if let Some(home) = home
            && let TrajectorySourceGuard::ObservedDocument(prior) = write.guard
            && let Some(steps) = prior.desktop_steps
            && write.document.source_revision.file.is_some()
        {
            desktop_declarations::record(
                transaction,
                &row.session,
                home.as_os_str().as_encoded_bytes(),
                write.document.source_revision,
                steps,
                prior.desktop_generation,
                Some(row),
            )?;
        }
        if write.document.origin != SessionTrajectoryOrigin::ImportedBundle
            && !accepted
            && !write.may_publish_unchanged_locator(transaction)?
        {
            return Ok(false);
        }
        if desktop_summaries_unavailable {
            let retained = retain_desktop_summaries(
                transaction,
                home.ok_or(StoreError::InvalidSessionReference)?,
                std::slice::from_ref(row),
                &MissingDesktopSummaries::All,
            )?;
            write_indexed_session(transaction, &retained[0])?;
        } else {
            write_indexed_session(transaction, row)?;
        }
        if let Some(home) = home {
            record_discovery_home(transaction, &row.session, home)?;
        }
        Ok(true)
    }
}

/// Reads indexed text and its source identities from a single `SQLite` snapshot.
pub struct TrajectorySearchSnapshot<'a> {
    connection: &'a Connection,
}

impl TrajectorySearchSnapshot<'_> {
    /// Reads cached session metadata and derived titles in the current search snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the cached metadata or titles cannot be read.
    pub fn cached_sessions_with_titles(
        &self,
        provider: Option<Provider>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        self.read_cached_sessions_with_titles(provider, false, None)
    }

    /// Reads cached sessions while limiting IDE entries to their verified home.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog or ownership records cannot be read.
    pub fn cached_sessions_with_titles_for_ide_home(
        &self,
        provider: Option<Provider>,
        ide_home: Option<&Path>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        let (mut sessions, mut titles) = self.cached_sessions_with_titles(provider)?;
        if provider.is_some_and(|provider| provider != Provider::AntigravityIde) {
            return Ok((sessions, titles));
        }
        let mut statement = self.connection.prepare_cached(
            "SELECT session_id FROM native_catalog_homes WHERE provider='antigravity-ide' AND verified AND home=?1",
        ).map_err(database_error)?;
        let allowed: std::collections::HashSet<String> = statement
            .query_map(
                [ide_home.map(|home| home.as_os_str().as_encoded_bytes())],
                |row| row.get(0),
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)?;
        let selected = |session: &SessionRef| {
            session.provider != Provider::AntigravityIde || allowed.contains(&session.id)
        };
        sessions.retain(|entry| selected(&entry.session));
        titles.retain(|entry| selected(&entry.session));
        Ok((sessions, titles))
    }

    fn read_cached_sessions_with_titles(
        &self,
        provider: Option<Provider>,
        picker: bool,
        homes: Option<[Option<&Path>; 3]>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        let title = if picker {
            "CASE WHEN catalog.name_observable THEN catalog.title END"
        } else {
            "catalog.title"
        };
        let connection = self.connection;
        let provider = provider.map(|provider| provider.to_string());
        let home_bytes =
            homes.map(|paths| paths.map(|path| path.map(|p| p.as_os_str().as_encoded_bytes())));
        let (filter, parameters): (&str, &[&dyn rusqlite::ToSql]) = match (&provider, &home_bytes) {
            (Some(provider), _) => ("WHERE catalog.provider=?1", &[provider]),
            (_, Some([antigravity, desktop, ide])) => (
                "WHERE (catalog.provider <> 'antigravity-ide' OR EXISTS (
                    SELECT 1 FROM native_catalog_homes AS native WHERE native.provider=catalog.provider
                    AND native.session_id=catalog.session_id AND native.verified AND native.home=?3))
                 AND NOT EXISTS (SELECT 1 FROM native_catalog_homes AS native
                 WHERE native.provider=catalog.provider AND native.session_id=catalog.session_id
                   AND native.verified AND native.home IS NOT CASE native.provider
                     WHEN 'antigravity' THEN ?1 WHEN 'antigravity-desktop' THEN ?2 WHEN 'antigravity-ide' THEN ?3 END)",
                &[antigravity, desktop, ide],
            ),
            _ => ("", &[]),
        };
        // Older writers can refresh the catalog without recording the selected file. When
        // cached search borrows its document, use the workspace established for that document.
        let sql = format!(
            "SELECT catalog.provider,catalog.session_id,{title},
               CASE WHEN pending.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NOT NULL
                    THEN w.project_path ELSE catalog.project_path END,
               CASE WHEN pending.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NOT NULL
                    THEN w.git_branch ELSE catalog.git_branch END,
               catalog.created_at,catalog.updated_at,catalog.updated_at_approximate,catalog.event_count,
               catalog.source_file_id,catalog.source_contents_id
             FROM scoped_session_catalog catalog LEFT JOIN session_trajectories trajectories ON catalog.provider='codex'
               AND catalog.source_file_id IS NULL AND catalog.source_contents_id IS NULL
               AND trajectories.provider=catalog.provider AND trajectories.session_id=catalog.session_id
               AND trajectories.origin='native'
             LEFT JOIN trajectory_index_failures failed ON {CONFLICTING_CODEX_FAILURE}
             LEFT JOIN codex_indexed_workspaces w ON failed.session_id IS NULL AND w.trajectory_id=trajectories.id
               AND w.source_file_id=trajectories.source_file_id AND w.source_contents_id=trajectories.source_contents_id
             LEFT JOIN codex_workspace_rechecks pending ON failed.session_id IS NULL AND pending.trajectory_id=trajectories.id
               AND pending.source_file_id=trajectories.source_file_id AND pending.source_contents_id=trajectories.source_contents_id
             {filter}
             ORDER BY catalog.updated_at IS NULL,catalog.updated_at DESC,catalog.provider,catalog.session_id"
        );
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let sessions: Vec<IndexedSession> = statement
            .query_map(parameters, indexed_session_from_row)
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(|_| StoreError::CorruptStore)?;
        let mut titles = self.read_trajectory_titles(true)?;
        if let Some(homes) = homes {
            for (provider, home) in [
                Provider::Antigravity,
                Provider::AntigravityDesktop,
                Provider::AntigravityIde,
            ]
            .into_iter()
            .zip(homes)
            {
                let Some(home) = home else {
                    titles.retain(|row| row.session.provider != provider);
                    continue;
                };
                let files: HashMap<_, _> = sessions
                    .iter()
                    .filter(|row| row.session.provider == provider)
                    .map(|row| {
                        (
                            row.session.id.as_str(),
                            row.source_revision.and_then(|rev| rev.file),
                        )
                    })
                    .collect();
                let sources = files
                    .iter()
                    .map(|(&id, &file)| (id, file))
                    .collect::<Vec<_>>();
                let conflicts = self.conflicting_native_documents(provider, home, &sources)?;
                titles.retain(|row| {
                    row.session.provider != provider
                        || (files.contains_key(row.session.id.as_str())
                            && !conflicts.contains(&row.session))
                });
                let scoped = conflicts
                    .into_iter()
                    .map(|session| {
                        let file = files[session.id.as_str()];
                        (session, Some(home.to_owned()), file)
                    })
                    .collect::<Vec<_>>();
                titles.extend(self.scoped_native_titles(&scoped)?);
            }
        }
        Ok((sessions, titles))
    }

    /// Returns sessions whose indexed document belongs to a different native home or selected file.
    ///
    /// # Errors
    ///
    /// Returns an error when a recorded native identity cannot be read.
    pub fn conflicting_native_documents(
        &self,
        provider: Provider,
        home: &Path,
        sources: &[(&str, Option<Uuid>)],
    ) -> Result<std::collections::HashSet<SessionRef>> {
        let selected = sources
            .iter()
            .map(|(id, file)| (*id, file.map(|uuid| uuid.simple().to_string())))
            .collect::<Vec<_>>();
        self.document_conflicts(provider, Some(home), &selected)
    }

    /// Returns cached sessions whose catalog and native search document cannot be used together.
    ///
    /// # Errors
    ///
    /// Returns an error when the cached ownership records cannot be read.
    pub fn conflicting_cached_documents(
        &self,
        provider: Provider,
        ids: &[&str],
    ) -> Result<std::collections::HashSet<SessionRef>> {
        let selected = ids.iter().map(|&id| (id, None)).collect::<Vec<_>>();
        self.document_conflicts(provider, None, &selected)
    }

    fn document_conflicts(
        &self,
        provider: Provider,
        home: Option<&Path>,
        selected: &[(&str, Option<String>)],
    ) -> Result<std::collections::HashSet<SessionRef>> {
        if selected.is_empty() || !TrajectorySourceGuard::uses_catalog_home(provider) {
            return Ok(std::collections::HashSet::new());
        }
        let selected =
            serde_json::to_string(selected).map_err(|_| StoreError::InvalidSessionReference)?;
        // Each candidate needs only primary-key lookups. A live read can identify a document
        // by its file. An offline read can also use a legacy document with its original locator.
        let mut statement = self.connection.prepare_cached(
            "SELECT docs.session_id FROM json_each(?1) AS picked
             CROSS JOIN session_trajectories AS docs ON docs.provider = ?2 AND docs.session_id = (picked.value ->> 0)
             LEFT JOIN session_index AS catalog ON catalog.provider = docs.provider AND catalog.session_id = docs.session_id
             LEFT JOIN native_catalog_homes AS native ON native.provider = docs.provider AND native.session_id = docs.session_id
             LEFT JOIN native_search_document_homes AS origin ON origin.trajectory_id = docs.id AND origin.source_file = docs.source_file_id
             WHERE docs.origin = 'native' AND CASE
                WHEN ?3 IS NULL AND native.home IS NULL AND origin.source_home IS NULL THEN
                    EXISTS (SELECT 1 FROM detached_native_documents WHERE trajectory_id = docs.id)
                WHEN ?3 IS NULL THEN coalesce(native.verified, 0) = 0 OR native.home IS NOT origin.source_home
                    OR origin.verification < 0 OR (catalog.source_file_id IS NOT NULL AND catalog.source_file_id IS NOT docs.source_file_id)
                WHEN origin.verification = 1 AND (picked.value ->> 1) IS NOT NULL THEN
                    origin.source_home IS NOT ?3 OR docs.source_file_id IS NOT unhex(picked.value ->> 1)
                WHEN origin.verification = 1 THEN origin.source_home IS NOT ?3 OR native.home IS NOT ?3
                    OR coalesce(native.verified, 0) = 0 OR (catalog.source_file_id IS NOT NULL AND catalog.source_file_id IS NOT docs.source_file_id)
                WHEN native.verified AND native.home <> ?3 THEN 1
                ELSE docs.source_file_id IS NULL OR hex(docs.source_file_id) IS NOT upper(picked.value ->> 1)
             END"
        ).map_err(database_error)?;
        let rows = statement
            .query_map(
                params![
                    selected,
                    provider.to_string(),
                    home.map(|path| path.as_os_str().as_encoded_bytes())
                ],
                |row| Ok(SessionRef::new(provider, row.get::<_, String>(0)?)),
            )
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|_| StoreError::CorruptStore)
    }

    /// Reads the derived title of the document selected by native home and file.
    ///
    /// # Errors
    ///
    /// Returns an error if the index or source identity cannot be read.
    pub fn native_source_title(
        &self,
        session: &SessionRef,
        home: &Path,
        file: Option<Uuid>,
    ) -> Result<Option<String>> {
        if self
            .conflicting_native_documents(session.provider, home, &[(&session.id, file)])?
            .is_empty()
        {
            return self.trajectory_title(session);
        }
        Ok(self
            .scoped_native_titles(&[(session.clone(), Some(home.to_owned()), file)])?
            .pop()
            .and_then(|row| row.title))
    }

    /// Reads the derived titles of scoped documents selected for conversation search.
    ///
    /// # Errors
    ///
    /// Returns an error if a source identity is invalid or the index cannot be read.
    pub fn scoped_native_titles(
        &self,
        sources: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
    ) -> Result<Vec<IndexedTrajectoryTitle>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT provider,session_id,derived_title,source_file,source_contents
             FROM scoped_native_documents WHERE derived_title IS NOT NULL AND id IN ({})",
            scoped_native_search::scoped_selection_sql(1)
        );
        let selected = scoped_native_search::scoped_selection_json(sources)?;
        let mut statement = self
            .connection
            .prepare_cached(&sql)
            .map_err(database_error)?;
        statement
            .query_map([selected], indexed_title_from_row)
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
            .indexed_trajectory_title(session)?
            .and_then(|entry| entry.title))
    }

    /// Returns native Codex documents whose catalog row belongs to another rollout revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the index cannot be read.
    pub fn conflicting_codex_catalog_rows(&self) -> Result<std::collections::HashSet<SessionRef>> {
        let connection = self.connection;
        let sql = format!(
            "SELECT trajectories.session_id FROM session_trajectories AS trajectories
                          INNER JOIN session_index AS catalog ON {CONFLICTING_CODEX_CATALOG}"
        );
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        statement
            .query_map([], |row| {
                Ok(SessionRef::new(Provider::Codex, row.get::<_, String>(0)?))
            })
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns the stored title or the source of a failure that hides it.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reference or if the store cannot be read.
    pub fn indexed_trajectory_title(
        &self,
        session: &SessionRef,
    ) -> Result<Option<IndexedTrajectoryTitle>> {
        validate_session_ref(session)?;
        let sql = format!(
            "{} WHERE trajectories.provider = ?1 AND trajectories.session_id = ?2",
            indexed_titles_sql()
        );
        self.connection
            .query_row(
                &sql,
                params![session.provider.to_string(), session.id],
                indexed_title_from_row,
            )
            .optional()
            .map_err(database_error)
    }

    /// Returns titles and source revisions from the same read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read.
    pub fn trajectory_titles_with_sources(&self) -> Result<Vec<IndexedTrajectoryTitle>> {
        self.read_trajectory_titles(false)
    }

    fn read_trajectory_titles(&self, scoped: bool) -> Result<Vec<IndexedTrajectoryTitle>> {
        let connection = self.connection;
        let codex = if scoped {
            "trajectories.provider='codex' AND EXISTS (SELECT 1 FROM scoped_session_catalog s
               WHERE s.provider=trajectories.provider AND s.session_id=trajectories.session_id)"
        } else {
            "trajectories.provider='codex'"
        };
        let sql = format!(
            "{} WHERE ({codex}) OR (trajectories.provider<>'codex' AND length(trajectories.derived_title)>0)",
            indexed_titles_sql()
        );
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        statement
            .query_map([], indexed_title_from_row)
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns redacted titles derived from searchable conversations.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read or contains invalid data.
    pub fn trajectory_titles(&self) -> Result<std::collections::HashMap<SessionRef, String>> {
        Ok(self
            .trajectory_titles_with_sources()?
            .into_iter()
            .filter_map(|entry| entry.title.map(|title| (entry.session, title)))
            .collect())
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
        self.search_selected_source_documents(query, limit, eligible_sessions, &[])
    }

    /// Ranks eligible primary and scoped documents together in the shared chunk index.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid references or an unreadable index.
    pub fn search_selected_source_documents(
        &self,
        query: &str,
        limit: usize,
        eligible_sessions: &[TrajectorySelection],
        scoped: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
    ) -> Result<SessionTrajectorySearchPage> {
        self.search_projected_source_documents(query, limit, eligible_sessions, scoped, &[])
    }

    /// Ranks verified alias documents with ordinary documents under their selected session references.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid references or an unreadable index.
    pub fn search_projected_source_documents(
        &self,
        query: &str,
        limit: usize,
        eligible_sessions: &[TrajectorySelection],
        scoped: &[(SessionRef, Option<PathBuf>, Option<Uuid>)],
        aliases: &[&CachedDatabaseAliasSource],
    ) -> Result<SessionTrajectorySearchPage> {
        for selection in eligible_sessions {
            validate_session_ref(&selection.session)?;
        }
        if eligible_sessions.is_empty() && scoped.is_empty() && aliases.is_empty() {
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
        let scoped_json = (!scoped.is_empty() || !aliases.is_empty())
            .then(|| scoped_native_search::scoped_selection_json(scoped))
            .transpose()?;
        self.search_session_trajectory_page_with_eligibility(
            query,
            limit,
            Some(&eligibility_json),
            scoped_json.as_deref(),
            (!aliases.is_empty()).then_some(aliases),
        )
    }
    fn search_session_trajectory_page_with_eligibility(
        &self,
        query: &str,
        limit: usize,
        eligibility_json: Option<&str>,
        scoped_json: Option<&str>,
        aliases: Option<&[&CachedDatabaseAliasSource]>,
    ) -> Result<SessionTrajectorySearchPage> {
        let clauses = trajectory_clauses(query);
        if clauses.is_empty() || limit == 0 {
            return Ok(empty_trajectory_search_page());
        }
        let limit = limit.min(TRAJECTORY_SEARCH_RESULT_LIMIT);
        let query_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let statement_sql =
            trajectory_search_page_sql(&clauses, scoped_json.is_some(), aliases.is_some());
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
        if let Some(json) = scoped_json {
            parameters.push(SqlValue::Text(json.to_owned()));
        }
        if let Some(aliases) = aliases {
            let aliases: Vec<_> = aliases
                .iter()
                .map(|source| (source.document_id, source.provider))
                .collect();
            parameters.push(SqlValue::Text(
                serde_json::to_string(&aliases).map_err(|_| StoreError::CorruptStore)?,
            ));
        }
        let connection = self.connection;
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
        let matches = with_trajectory_excerpts(connection, &clauses, rows)?;
        Ok(SessionTrajectorySearchPage { matches, has_more })
    }
}

impl Store {
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
                    source_file_id BLOB CHECK (source_file_id IS NULL OR length(source_file_id) = 16),
                    source_contents_id BLOB CHECK (source_contents_id IS NULL OR length(source_contents_id) = 16),
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
        ensure_session_index_columns(&transaction)?;
        initialize_bundle_source_index(&transaction)?;
        initialize_trajectory_schema(&transaction)?;
        initialize_discovery_schema(&transaction)?;
        primary_legacy::initialize(&transaction)?;
        scoped_native_documents::initialize(&transaction)?;
        desktop_declarations::initialize(&transaction)?;
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

fn indexed_titles_sql() -> String {
    format!("SELECT trajectories.provider, trajectories.session_id,
                CASE WHEN failed.session_id IS NULL AND catalog.session_id IS NULL
                     THEN nullif(trajectories.derived_title, '') END,
                CASE WHEN catalog.session_id IS NOT NULL THEN catalog.source_file_id
                     WHEN failed.session_id IS NOT NULL THEN failed.source_file_id ELSE trajectories.source_file_id END,
                CASE WHEN catalog.session_id IS NOT NULL THEN catalog.source_contents_id
                     WHEN failed.session_id IS NOT NULL THEN failed.source_contents_id ELSE trajectories.source_contents_id END
             FROM session_trajectories AS trajectories
             LEFT JOIN trajectory_index_failures AS failed ON {CONFLICTING_CODEX_FAILURE}
             LEFT JOIN session_index AS catalog ON {CONFLICTING_CODEX_CATALOG}")
}

fn indexed_title_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedTrajectoryTitle> {
    let provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    Ok(IndexedTrajectoryTitle {
        session: SessionRef::new(provider, row.get::<_, String>(1)?),
        title: row.get(2)?,
        source_revision: SourceRevision {
            file: row.get::<_, Option<[u8; 16]>>(3)?.map(Uuid::from_bytes),
            contents: row.get::<_, Option<[u8; 16]>>(4)?.map(Uuid::from_bytes),
            ..Default::default()
        },
    })
}

/// Builds one ranked page statement.
///
/// Parameters are `?1..?N` FTS expressions, `?N+1` eligibility JSON, `?N+2` limit, then one folded
/// phrase per exact clause, in clause order. Rows end with the snippet's chunk ID and clause index.
/// Exact clauses filter candidate chunks with [`FOLDED_CONTAINS_FUNCTION`] and leave the snippet
/// empty for the caller to cut around the phrase.
fn trajectory_search_page_sql(
    clauses: &[TrajectoryClause],
    scoped: bool,
    projected: bool,
) -> String {
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
    let primary = if projected {
        "direct_primary"
    } else {
        "eligible"
    };
    let mut eligible = format!(
        "WITH {primary}(provider, session_id, file, contents) AS (
             SELECT provider, session_id, NULL, NULL FROM session_trajectories
             WHERE ?{eligibility_parameter} IS NULL
             UNION ALL
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]'),
                    json_extract(value, '$[2]'), json_extract(value, '$[3]')
             FROM json_each(?{eligibility_parameter})
             WHERE ?{eligibility_parameter} IS NOT NULL
         ),"
    );
    let documents = if projected {
        let selected = scoped_native_search::scoped_selection_sql(exact_parameter + 1);
        let parameter = exact_parameter + 2;
        write!(eligible, "
            projection AS (SELECT value ->> 0 AS id, value ->> 1 AS provider FROM json_each(?{parameter})),
            direct_scoped AS MATERIALIZED ({selected}),
            eligible(provider,session_id,file,contents) AS (
                SELECT * FROM direct_primary UNION
                SELECT t.provider,t.session_id,NULL,NULL FROM projection p JOIN session_trajectories t ON t.id=p.id
            ),
            selected_scoped AS (SELECT id FROM direct_scoped UNION SELECT -id FROM projection WHERE id<0),
            selected_documents(id,provider) AS (
                SELECT t.id,t.provider FROM direct_primary e JOIN session_trajectories t USING(provider,session_id)
                UNION ALL SELECT -s.id,s.provider FROM direct_scoped e JOIN scoped_native_documents s USING(id)
                UNION ALL SELECT id,provider FROM projection
            ),
            documents AS (
                SELECT t.id,e.provider,t.session_id,t.source_complete,t.complete,t.indexed_byte_count,
                       t.source_byte_count,t.truncation_strategy,t.source_updated_at
                FROM selected_documents e JOIN session_trajectories t ON t.id=e.id
                UNION ALL
                SELECT -s.id,e.provider,s.session_id,s.source_complete,s.complete,s.indexed_byte_count,
                       s.source_byte_count,s.truncation_strategy,s.source_updated_at
                FROM selected_documents e JOIN scoped_native_documents s ON -e.id=s.id
            ),").expect("writing SQL into a string cannot fail");
        "documents"
    } else if scoped {
        let selected = scoped_native_search::scoped_selection_sql(exact_parameter + 1);
        write!(
            eligible,
            " selected_scoped AS MATERIALIZED ({selected}),
              documents AS (
                SELECT t.id, t.provider, t.session_id, t.source_complete, t.complete, t.indexed_byte_count,
                       t.source_byte_count, t.truncation_strategy, t.source_updated_at
                FROM session_trajectories t WHERE (provider, session_id) IN (SELECT provider, session_id FROM eligible)
                UNION ALL
                SELECT -s.id, s.provider, s.session_id, s.source_complete, s.complete, s.indexed_byte_count,
                       s.source_byte_count, s.truncation_strategy, s.source_updated_at
                FROM selected_scoped e JOIN scoped_native_documents s ON s.id=e.id
              ),"
        ).expect("writing SQL into a string cannot fail");
        "documents"
    } else {
        "session_trajectories"
    };
    if let [clause] = clauses {
        single_clause_page_sql(
            eligible,
            clause,
            &exact_filters[0],
            limit_parameter,
            scoped,
            documents,
        )
    } else {
        multi_clause_page_sql(
            eligible,
            clauses,
            &exact_filters,
            limit_parameter,
            scoped,
            documents,
        )
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
                .prepare("SELECT stored_text FROM session_trajectory_chunk_rows WHERE id = ?1 UNION ALL SELECT stored_text FROM scoped_native_chunk_rows WHERE id = -?1")
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

/// Both chunk tables use the same FTS5 index. Scoped chunks have negative FTS row IDs.
fn matching_chunks_sql(
    clause: &TrajectoryClause,
    parameter: usize,
    exact_filter: &str,
    scoped: bool,
) -> String {
    let primary = trajectory_candidate_chunks_sql(clause);
    let mut sql = format!(
        "SELECT chunks.id AS chunk_id, chunks.trajectory_id, chunks.chunk_index,
                session_trajectory_chunks_fts.rank AS match_rank
         {primary}
         WHERE session_trajectory_chunks_fts MATCH ?{parameter}{exact_filter}"
    );
    if scoped {
        write!(sql,
            " UNION ALL
              SELECT -chunks.id, -chunks.document_id, chunks.chunk_index, session_trajectory_chunks_fts.rank
              FROM session_trajectory_chunks_fts
              JOIN scoped_native_chunk_rows chunks ON chunks.id = -session_trajectory_chunks_fts.rowid
              JOIN selected_scoped e ON e.id = chunks.document_id
              WHERE session_trajectory_chunks_fts MATCH ?{parameter}{exact_filter}"
        ).expect("writing SQL into a string cannot fail");
    }
    sql
}

fn single_clause_page_sql(
    mut sql: String,
    clause: &TrajectoryClause,
    exact_filter: &str,
    limit_parameter: usize,
    scoped: bool,
    documents: &str,
) -> String {
    let candidates = matching_chunks_sql(clause, 1, exact_filter, scoped);
    write!(
        sql,
        "ranked_chunks AS (
             SELECT chunk_id, trajectory_id, match_rank,
                    row_number() OVER (
                        PARTITION BY trajectory_id
                        ORDER BY match_rank, chunk_index
                    ) AS chunk_rank
             FROM ({candidates})
         )
         SELECT trajectories.provider, trajectories.session_id,
                trajectories.source_complete, trajectories.complete,
                trajectories.indexed_byte_count, trajectories.source_byte_count,
                trajectories.truncation_strategy, ranked_chunks.chunk_id, 0
         FROM ranked_chunks
         INNER JOIN {documents} AS trajectories
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
    scoped: bool,
    documents: &str,
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
        let candidates = matching_chunks_sql(clause, parameter, exact_filter, scoped);
        write!(
            sql,
            "SELECT trajectory_id, chunk_id, {clause_index}, match_rank, {exact_clause}
             FROM ({candidates})"
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
             SELECT qualified_trajectories.trajectory_id, trajectories.provider,
                    qualified_trajectories.trajectory_rank
             FROM qualified_trajectories
             INNER JOIN {documents} AS trajectories
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
             WHERE clause_rank_order = 1 AND trajectory_id IN (SELECT trajectory_id FROM ranked_trajectories)
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
         INNER JOIN {documents} AS trajectories
             ON trajectories.id = ranked_trajectories.trajectory_id AND trajectories.provider=ranked_trajectories.provider
         ORDER BY ranked_trajectories.trajectory_rank,
                  trajectories.source_updated_at DESC,
                  trajectories.provider, trajectories.session_id"
    )
    .expect("writing SQL into a string cannot fail");
    sql
}

fn validate_discovered_sessions(provider: Provider, sessions: &[IndexedSession]) -> Result<()> {
    if sessions
        .iter()
        .any(|session| session.session.provider != provider || session.session.id.is_empty())
    {
        return Err(StoreError::InvalidSessionReference);
    }
    if sessions.iter().any(|session| {
        session
            .project_path
            .as_deref()
            .is_some_and(|path| path.to_str().is_none())
    }) {
        return Err(StoreError::InvalidWorkspaceRoot);
    }
    Ok(())
}

fn record_verified_document_home(
    transaction: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
    context: TrajectoryWriteContext<'_>,
) -> Result<()> {
    let Some(file) = write.document.source_revision.file else {
        return Ok(());
    };
    let TrajectorySourceGuard::ObservedDocument(observation) = write.guard else {
        return Ok(());
    };
    let explicit_home = matches!(
        context,
        TrajectoryWriteContext::Locator { home: Some(_), .. }
    );
    let observed_home = matches!(observation.catalog, CatalogObservation::Verified { .. });
    if !matches!(
        write.session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop | Provider::AntigravityIde
    ) || !(explicit_home || observed_home)
    {
        return Ok(());
    }
    if !explicit_home && let Some(steps) = observation.desktop_steps {
        let home: Vec<u8> = transaction
            .query_row(
                "SELECT home FROM native_catalog_homes WHERE provider=?2
             AND session_id=?1 AND verified",
                params![write.session.id, write.session.provider.to_string()],
                |r| r.get(0),
            )
            .map_err(database_error)?;
        desktop_declarations::record(
            transaction,
            write.session,
            &home,
            write.document.source_revision,
            steps,
            observation.desktop_generation,
            None,
        )?;
    }
    let changed = transaction.prepare_cached(
        "INSERT INTO native_search_document_homes(trajectory_id,source_file,source_home,verification)
         SELECT docs.id, docs.source_file_id, native.home, 1 FROM session_trajectories AS docs
         JOIN native_catalog_homes AS native USING(provider,session_id)
         WHERE docs.provider = ?1 AND docs.session_id = ?2 AND docs.origin = 'native' AND native.verified
           AND docs.source_file_id = ?3 AND docs.source_contents_id IS ?4 AND docs.missing_ancestor_id IS ?5
         ON CONFLICT(trajectory_id) DO UPDATE SET source_file = excluded.source_file, source_home = excluded.source_home, verification = 1
         WHERE source_file <> excluded.source_file OR source_home <> excluded.source_home OR verification <> 1"
    ).and_then(|mut statement| statement.execute(params![
        write.session.provider.to_string(), write.session.id, file.into_bytes(),
        write.document.source_revision.contents.map(Uuid::into_bytes),
        write.document.source_revision.missing_ancestor.map(Uuid::into_bytes),
    ])).map_err(database_error)?;
    if changed != 0 {
        transaction.prepare_cached(
            "DELETE FROM scoped_native_documents WHERE provider=?1 AND session_id=?2 AND source_file=?3
             AND source_home=(SELECT home FROM native_catalog_homes WHERE provider=?1 AND session_id=?2 AND verified)",
        ).and_then(|mut statement| statement.execute(params![write.session.provider.to_string(),write.session.id,file.as_bytes()])).map_err(database_error)?;
    }
    record_primary_database_receipt(transaction, write)
}

fn record_primary_database_receipt(
    tx: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
) -> Result<()> {
    if !TrajectorySourceGuard::uses_catalog_home(write.session.provider)
        || write.document.origin != SessionTrajectoryOrigin::Native
    {
        return Ok(());
    }
    let (Some(file), Some(contents)) = (
        write.document.source_revision.file,
        write.document.source_revision.contents,
    ) else {
        return Ok(());
    };
    let home = tx
        .query_row(
            "SELECT origin.source_home FROM session_trajectories doc
         JOIN native_search_document_homes origin ON origin.trajectory_id=doc.id
         JOIN native_catalog_homes locator USING(provider,session_id)
         WHERE doc.provider=?7 AND doc.session_id=?1 AND doc.origin='native'
           AND origin.verification=1 AND origin.source_home=locator.home AND locator.verified
           AND origin.source_file=?2 AND doc.source_file_id=?2 AND doc.source_contents_id=?3
           AND doc.content_hash=?4 AND doc.document_version=?5 AND doc.source_complete=?6",
            params![
                write.session.id,
                file.as_bytes(),
                contents.as_bytes(),
                write.content_hash,
                write.document.document_version,
                write.source_complete,
                write.session.provider.to_string(),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(database_error)?;
    if let Some(home) = home {
        scoped_native_documents::database_receipts::save(
            tx,
            write.session,
            &home,
            file,
            contents,
            &write.content_hash,
            write.document.database_step_proof,
        )?;
        physical_database_bodies::record(tx, write, &home)?;
    }
    Ok(())
}

fn retain_desktop_summaries(
    connection: &Connection,
    home: &Path,
    sessions: &[IndexedSession],
    missing: &MissingDesktopSummaries,
) -> Result<Vec<IndexedSession>> {
    let Some(first) = sessions.first() else {
        return Ok(Vec::new());
    };
    let provider = first.session.provider;
    validate_discovered_sessions(provider, sessions)?;
    desktop_declarations::table(provider).ok_or(StoreError::InvalidSessionReference)?;
    let mut statement = connection
        .prepare_cached(
            "SELECT ?4,session_id,summary->>'title',summary->>'project_path',summary->>'git_branch',
                summary->>'created_at',summary->>'updated_at',summary->>'approximate',p.steps,source_file,NULL
         FROM physical_database_summaries d JOIN physical_step_declarations p USING(session_id,source_home,source_file) WHERE session_id=?1 AND source_home=?2 AND source_file=?3 AND summary IS NOT NULL
         UNION ALL SELECT catalog.provider, catalog.session_id, title, project_path, git_branch,
               created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
         FROM session_index AS catalog JOIN native_catalog_homes USING(provider,session_id)
         WHERE ?4='antigravity-desktop' AND catalog.provider = ?4 AND catalog.session_id = ?1
             AND verified AND home = ?2 LIMIT 1",
        )
        .map_err(database_error)?;
    sessions
        .iter()
        .map(|session| {
            if !missing.contains(&session.session.id) {
                return Ok(session.clone());
            }
            let file = session.source_revision.and_then(|revision| revision.file);
            let previous = statement
                .query_row(
                    params![
                        session.session.id,
                        home.as_os_str().as_encoded_bytes(),
                        file.map(Uuid::into_bytes),
                        provider.to_string()
                    ],
                    indexed_session_from_row,
                )
                .optional()
                .map_err(database_error)?;
            let Some(mut previous) = previous else {
                return Ok(session.clone());
            };
            if file.is_none() || file != previous.source_revision.and_then(|revision| revision.file)
            {
                previous.event_count = session.event_count;
            }
            previous.source_revision = session.source_revision;
            if previous.updated_at_approximate || previous.updated_at.is_none() {
                previous.updated_at = session.updated_at.or(previous.updated_at);
                previous.updated_at_approximate = true;
            }
            Ok(previous)
        })
        .collect()
}

fn qualify_legacy_document_homes(
    tx: &Transaction<'_>,
    home: &Path,
    sessions: &[IndexedSession],
    file_at: &dyn Fn(usize) -> Option<Uuid>,
) -> Result<()> {
    if sessions
        .first()
        .is_none_or(|row| !TrajectorySourceGuard::uses_catalog_home(row.session.provider))
        || !tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM native_search_document_homes WHERE verification < 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)?
    {
        return Ok(());
    }
    let mut prior = tx.prepare_cached(
        "SELECT origin.trajectory_id FROM session_trajectories AS doc
         JOIN native_search_document_homes AS origin ON origin.trajectory_id=doc.id AND origin.source_file=doc.source_file_id
         WHERE doc.provider=?1 AND doc.session_id=?2 AND origin.verification < 1"
    ).map_err(database_error)?;
    let mut qualify = tx
        .prepare_cached(
            "UPDATE native_search_document_homes SET
            verification=CASE WHEN source_file=?2 THEN 1 ELSE -1 END,
            source_home=CASE WHEN source_file=?2 THEN ?3 ELSE source_home END
         WHERE trajectory_id=?1 AND (verification=0 OR source_file=?2)",
        )
        .map_err(database_error)?;
    for (position, row) in sessions.iter().enumerate() {
        let id = prior
            .query_row(
                params![row.session.provider.to_string(), row.session.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)?;
        if let Some(id) = id {
            qualify
                .execute(params![
                    id,
                    file_at(position).map(Uuid::into_bytes),
                    home.as_os_str().as_encoded_bytes()
                ])
                .map_err(database_error)?;
        }
    }
    Ok(())
}

fn record_discovery_home(
    transaction: &Transaction<'_>,
    session: &SessionRef,
    home: &Path,
) -> Result<()> {
    if !matches!(
        session.provider,
        Provider::Antigravity | Provider::AntigravityDesktop | Provider::AntigravityIde
    ) {
        return Err(StoreError::InvalidSessionReference);
    }
    transaction
        .prepare_cached(
            "INSERT INTO native_catalog_homes(provider,session_id,home) VALUES(?1,?2,?3)
         ON CONFLICT(provider,session_id) DO UPDATE SET home=excluded.home, verified=1
         WHERE native_catalog_homes.home IS NOT excluded.home OR NOT native_catalog_homes.verified",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                session.provider.to_string(),
                session.id,
                home.as_os_str().as_encoded_bytes()
            ])
        })
        .map_err(database_error)?;
    Ok(())
}

fn initialize_catalog_home(connection: &Connection) -> Result<()> {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))
        .and_then(|path| codex_home_key(&path));
    connection.execute_batch(
        "CREATE TEMP TABLE current_codex_home (home TEXT);
         CREATE TEMP VIEW raw_session_catalog AS
         SELECT c.*, (c.provider <> 'codex' OR c.codex_home = h.home) IS 1 AS home_matches,
             (c.provider <> 'codex' OR (c.codex_home = h.home AND NOT c.codex_legacy_name)) IS 1 AS name_observable,
             (c.provider='codex' AND h.home IS NOT NULL AND (c.codex_home IS NULL OR c.codex_legacy_name)
              AND c.title IS NOT NULL AND c.source_file_id IS NOT NULL AND c.source_contents_id IS NOT NULL
              AND c.updated_at IS NOT NULL AND NOT c.updated_at_approximate AND NOT EXISTS (
                 SELECT 1 FROM main.session_trajectories t WHERE t.provider=c.provider AND t.session_id=c.session_id
                   AND (t.origin<>'native' OR t.source_file_id IS NOT c.source_file_id))) AS unscoped_name,
             CASE WHEN c.provider='codex' AND c.codex_home IS NULL THEN (
                 SELECT d.origin='native' AND c.source_file_id IS NOT NULL AND c.source_contents_id IS NOT NULL
                    AND d.source_file_id IS c.source_file_id AND d.source_contents_id IS c.source_contents_id
                    AND c.updated_at IS NOT NULL AND NOT c.updated_at_approximate AND d.source_updated_at IS c.updated_at
                 FROM main.session_trajectories d WHERE d.provider=c.provider AND d.session_id=c.session_id
             ) END AS matches_unscoped_document
         FROM main.session_index c CROSS JOIN current_codex_home h
         WHERE c.provider <> 'codex' OR (h.home IS NOT NULL AND (c.codex_home IS NULL OR c.codex_home = h.home));
         CREATE TEMP VIEW scoped_session_catalog AS SELECT * FROM raw_session_catalog WHERE home_matches;"
    ).map_err(database_error)?;
    connection
        .execute("INSERT INTO current_codex_home VALUES (?1)", [home])
        .map_err(database_error)?;
    Ok(())
}

fn codex_home_key(path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).ok()?;
    let resolved = absolute.ancestors().find_map(|ancestor| {
        let canonical = fs::canonicalize(ancestor).ok()?;
        let suffix = absolute.strip_prefix(ancestor).ok()?;
        Some(if suffix.as_os_str().is_empty() {
            canonical
        } else {
            canonical.join(suffix)
        })
    })?;
    resolved.into_os_string().into_string().ok()
}

fn indexed_provider_rows(
    connection: &Connection,
    provider: Provider,
) -> Result<Vec<IndexedSession>> {
    let mut statement = connection
            .prepare(
                "
                SELECT provider, session_id, title, project_path, git_branch,
                       created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
                FROM scoped_session_catalog
                WHERE provider = ?1
                ORDER BY updated_at IS NULL, updated_at DESC, session_id
                ",
            )
            .map_err(database_error)?;
    let rows = statement
        .query_map([provider.to_string()], indexed_session_from_row)
        .map_err(database_error)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| StoreError::CorruptStore)
}

fn same_codex_catalog_source(left: &IndexedSession, right: &IndexedSession) -> bool {
    left.updated_at.is_some()
        && left.updated_at == right.updated_at
        && left.source_revision.is_some_and(|revision| {
            revision.file.is_some()
                && revision.contents.is_some()
                && right
                    .source_revision
                    .is_some_and(|other| revision.matches_file_contents(other))
        })
}

fn check_discovered_catalog<'a>(
    connection: &Connection,
    context: TrajectoryWriteContext<'a>,
) -> Result<std::collections::HashSet<&'a SessionRef>> {
    let mut unowned = std::collections::HashSet::new();
    let TrajectoryWriteContext::Discovered(observed) = context else {
        return Ok(unowned);
    };
    for selected in observed {
        let (current, legacy) = selected_catalog_rows(connection, &selected.session)?;
        if let Some(current) = current
            && same_codex_catalog_source(&current.metadata, selected)
            && current.metadata.title != selected.title
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        if legacy.is_some_and(|prior| prior.home.is_none() && prior.row == **selected) {
            unowned.insert(&selected.session);
        }
    }
    Ok(unowned)
}

const SELECTED_CATALOG_SQL: &str = "SELECT c.provider,c.session_id,c.title,c.project_path,c.git_branch,c.created_at,c.updated_at,
         c.updated_at_approximate,c.event_count,c.source_file_id,c.source_contents_id,c.codex_home,
         scoped.name_observable IS 1,scoped.unscoped_name IS 1,scoped.matches_unscoped_document,scoped.home_matches IS 1
         FROM session_index c LEFT JOIN (
             SELECT codex_home,name_observable,unscoped_name,matches_unscoped_document,home_matches
             FROM raw_session_catalog WHERE provider=?1 AND session_id=?2
         ) scoped ON scoped.codex_home IS c.codex_home
         WHERE c.provider=?1 AND c.session_id=?2
         ORDER BY scoped.home_matches DESC LIMIT 1";

fn selected_catalog_rows(
    connection: &Connection,
    session: &SessionRef,
) -> Result<(Option<ScopedCatalogRow>, Option<UntrustedCatalogRow>)> {
    let row = connection
        .query_row(
            SELECTED_CATALOG_SQL,
            params![session.provider.to_string(), session.id],
            |r| {
                let entry = indexed_session_from_row(r)?;
                if r.get::<_, bool>(12)? {
                    Ok((
                        Some(ScopedCatalogRow {
                            metadata: entry,
                            name_observable: true,
                            matches_unscoped_document: r.get(14)?,
                            home_matches: r.get(15)?,
                        }),
                        None,
                    ))
                } else {
                    Ok((
                        None,
                        Some(UntrustedCatalogRow {
                            row: entry,
                            home: r.get(11)?,
                            recoverable: r.get(13)?,
                        }),
                    ))
                }
            },
        )
        .optional()
        .map_err(database_error)?;
    Ok(row.unwrap_or_default())
}

fn unscoped_catalog_name(
    connection: &Connection,
    session: &SessionRef,
) -> Result<Option<IndexedSession>> {
    connection.query_row(
        "SELECT provider,session_id,title,project_path,git_branch,created_at,updated_at,updated_at_approximate,
         event_count,source_file_id,source_contents_id FROM raw_session_catalog
         WHERE provider='codex' AND session_id=?1 AND unscoped_name",
        [&session.id], indexed_session_from_row,
    ).optional().map_err(database_error)
}

fn same_legacy_catalog_file(old: &IndexedSession, selected: &IndexedSession) -> bool {
    old.session == selected.session
        && !old.updated_at_approximate
        && !selected.updated_at_approximate
        && old.updated_at.is_some()
        && selected.updated_at.is_some()
        && old
            .source_revision
            .zip(selected.source_revision)
            .is_some_and(|(a, b)| {
                a.file.is_some() && a.file == b.file && a.contents.is_some() && b.contents.is_some()
            })
}

fn corroborated_legacy_name<'a>(
    old: &'a IndexedSession,
    selected: &IndexedSession,
    assignment: &str,
) -> Option<&'a str> {
    if !same_legacy_catalog_file(old, selected) {
        return None;
    }
    old.title
        .as_deref()
        .filter(|name| omnis_core::redact_secrets(assignment) == **name)
}

fn unindexed_codex_name<'a>(
    prior: &'a IndexedSession,
    selected: &IndexedSession,
    document: Option<TrajectoryIndexState>,
) -> Option<&'a str> {
    let old = prior.source_revision?;
    let new = selected.source_revision?;
    if prior.session != selected.session
        || selected.session.provider != Provider::Codex
        || old.contents.is_none()
        || new.contents.is_none()
        || new.file.is_none()
        || old.file != new.file
        || document.is_some_and(|row| row.source_revision.file == new.file)
    {
        return None;
    }
    prior.title.as_deref()
}

// A legacy catalog and its document have independent timestamps. Check the observed document before removal.
const UNSTAMPED_CODEX_DOCUMENT: &str = "SELECT json_array(
    i.codex_home,d.id,d.source_updated_at,d.source_updated_submillis,d.indexed_at,hex(d.content_hash),d.document_version,
    d.source_complete,d.provider_limited,d.native_summary,d.complete,d.source_byte_count,d.indexed_byte_count,d.truncation_strategy,
    d.derived_title,d.derived_title_is_verified,d.derived_title_is_local_opener)
    FROM session_trajectories d JOIN session_index i ON i.provider=d.provider AND i.session_id=d.session_id
    WHERE d.provider='codex' AND d.origin='native' AND NOT d.protected_by_bundle
      AND i.source_file_id IS NULL AND i.source_contents_id IS NULL
      AND d.source_file_id IS NULL AND d.source_contents_id IS NULL AND d.missing_ancestor_id IS NULL
      AND i.codex_home=(SELECT home FROM current_codex_home)";

fn remove_codex_catalog_row(
    connection: &Connection,
    prior: &IndexedSession,
    unstamped: Option<&str>,
) -> Result<()> {
    let revision = prior.source_revision.unwrap_or_default();
    if let Some(receipt) = unstamped {
        let delete = format!(
            "DELETE FROM session_trajectories WHERE provider='codex' AND session_id=?1
            AND ?2=({UNSTAMPED_CODEX_DOCUMENT} AND d.session_id=?1)"
        );
        connection
            .prepare_cached(&delete)
            .map_err(database_error)?
            .execute(params![prior.session.id, receipt])
            .map_err(database_error)?;
    } else if revision.file.is_some() {
        connection
            .prepare_cached(
                "DELETE FROM session_trajectories WHERE provider='codex' AND session_id=?1
             AND origin='native' AND protected_by_bundle=0 AND source_file_id IS ?2
             AND source_contents_id IS ?3 AND source_updated_at IS ?4",
            )
            .map_err(database_error)?
            .execute(params![
                prior.session.id,
                revision.file.map(Uuid::into_bytes),
                revision.contents.map(Uuid::into_bytes),
                prior.updated_at.map(|t| t.timestamp_millis())
            ])
            .map_err(database_error)?;
    }
    connection
        .prepare_cached("DELETE FROM session_index WHERE provider='codex' AND session_id=?1")
        .map_err(database_error)?
        .execute([&prior.session.id])
        .map_err(database_error)?;
    Ok(())
}

struct ScopedCatalogRow {
    metadata: IndexedSession,
    name_observable: bool,
    matches_unscoped_document: Option<bool>,
    home_matches: bool,
}

impl ScopedCatalogRow {
    fn needs_update(&self, observed: Option<&IndexedSession>, selected: &IndexedSession) -> bool {
        if !self.home_matches && selected == &self.metadata {
            return observed == Some(selected) && self.matches_unscoped_document != Some(false);
        }
        self.verified_metadata() != Some(selected)
    }

    fn verified_metadata(&self) -> Option<&IndexedSession> {
        self.name_observable.then_some(&self.metadata)
    }
}

fn codex_refresh_inventory(
    connection: &Connection,
    incoming: &[IndexedSession],
    complete: bool,
) -> Result<Vec<ScopedCatalogRow>> {
    const SELECT: &str = "SELECT provider, session_id, title, project_path, git_branch,
        created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id,
        name_observable IS 1, matches_unscoped_document, home_matches
        FROM raw_session_catalog WHERE provider='codex' AND (SELECT home FROM current_codex_home) IS NOT NULL";
    let read = |row: &rusqlite::Row<'_>| {
        Ok(ScopedCatalogRow {
            metadata: indexed_session_from_row(row)?,
            name_observable: row.get(11)?,
            matches_unscoped_document: row.get(12)?,
            home_matches: row.get(13)?,
        })
    };
    if complete {
        let mut statement = connection.prepare_cached(SELECT).map_err(database_error)?;
        return statement
            .query_map([], read)
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error);
    }
    let mut statement = connection
        .prepare_cached(&format!("{SELECT} AND session_id=?1"))
        .map_err(database_error)?;
    incoming
        .iter()
        .filter_map(|row| {
            statement
                .query_row([&row.session.id], read)
                .optional()
                .transpose()
        })
        .collect::<rusqlite::Result<_>>()
        .map_err(database_error)
}

#[derive(Default)]
struct CodexCatalogChanges {
    updated: Vec<IndexedSession>,
    missing: Vec<IndexedSession>,
}

fn reconcile_codex_catalog(
    transaction: &Transaction<'_>,
    incoming: &[IndexedSession],
    names: CatalogNameObservations<'_>,
    complete: bool,
) -> Result<CodexCatalogChanges> {
    let names_complete = names.complete;
    let observed: HashMap<_, _> = names
        .observed
        .iter()
        .map(|row| (row.session.id.as_str(), row))
        .collect();
    let persisted = codex_refresh_inventory(transaction, incoming, complete)?;
    let mut current: HashMap<_, _> = persisted
        .iter()
        .map(|row| (row.metadata.session.id.as_str(), row))
        .collect();
    let mut pending = pending_codex_catalog_titles(transaction)?;
    let mut seen = std::collections::HashSet::new();
    let mut changes = CodexCatalogChanges::default();
    for incoming_row in incoming {
        let id = incoming_row.session.id.as_str();
        let prior = if complete {
            if !seen.insert(id) {
                return Err(StoreError::InvalidSessionReference);
            }
            current.remove(id)
        } else {
            current.get(id).copied()
        };
        let mut row = incoming_row.clone();
        let raw_old = prior.map(|prior| &prior.metadata);
        let observed_old = observed.get(id).copied();
        if raw_old != observed_old && raw_old != Some(&row) {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let old = prior.and_then(ScopedCatalogRow::verified_metadata);
        if old.is_none() && row.title.is_none() {
            reconcile_unowned_name(transaction, &mut row, names)?;
        }
        let mut new_pending = false;
        if let Some(prior) = old
            && row.title.is_none()
        {
            if same_codex_catalog_source(prior, &row) {
                if names_complete {
                    withdraw_catalog_name(transaction, prior, true)?;
                } else {
                    row.title.clone_from(&prior.title);
                }
            } else if !names_complete
                && prior.title.is_some()
                && let Some(name) = unindexed_codex_name(
                    prior,
                    &row,
                    cached_trajectory_state(transaction, &row.session)?,
                )
            {
                row.title = Some(name.to_owned());
            } else {
                new_pending = remember_provider_name(transaction, prior, &row)?;
            }
        }
        let existing_pending = pending.remove(id);
        if new_pending || existing_pending.is_some() {
            if names_complete
                || row.title.is_some()
                || (!new_pending && existing_pending == Some(false))
            {
                resolve_pending_catalog_title(transaction, &row, true)?;
            }
        } else if old.is_none()
            && row.title.is_none()
            && let Some((trajectory_id, title)) = uncataloged_trajectory_title(transaction, &row)?
        {
            // Older live searches saved the document without recording where its name came from.
            if names_complete {
                withdraw_trajectory_name(transaction, &row, &title, true)?;
            } else {
                transaction.execute(
                    "INSERT INTO codex_pending_catalog_titles (trajectory_id, source_file_id, source_contents_id, title)
                     SELECT id, source_file_id, source_contents_id, derived_title FROM session_trajectories WHERE id=?1",
                    [trajectory_id]).map_err(database_error)?;
            }
        }
        if prior.is_none_or(|prior| prior.needs_update(observed_old, &row)) {
            changes.updated.push(row);
        }
    }
    if complete {
        for (id, prior) in current {
            if observed.get(id).copied() != Some(&prior.metadata) {
                return Err(StoreError::TrajectorySourceChanged);
            }
            if prior.home_matches {
                changes.missing.push(prior.metadata.clone());
            }
        }
    }
    Ok(changes)
}

fn reconcile_unowned_name(
    transaction: &Transaction<'_>,
    row: &mut IndexedSession,
    names: CatalogNameObservations<'_>,
) -> Result<()> {
    if !names.complete
        && let Some(assignment) = names.prior_assignments.get(&row.session.id)
        && let Some(prior) = unscoped_catalog_name(transaction, &row.session)?
        && let Some(name) = corroborated_legacy_name(&prior, row, assignment)
    {
        row.title = Some(name.to_owned());
    } else {
        withdraw_other_home_name(transaction, row, true)?;
    }
    Ok(())
}

fn withdraw_other_home_name(
    connection: &Connection,
    selected: &IndexedSession,
    recheck: bool,
) -> Result<()> {
    let Some((file, contents)) = selected
        .source_revision
        .and_then(|r| r.file.zip(r.contents))
    else {
        return Ok(());
    };
    connection.execute(
        "UPDATE session_trajectories AS d SET derived_title=NULL, derived_title_is_verified=0,
            derived_title_is_local_opener=NULL, document_version=CASE WHEN ?3 THEN 0 ELSE document_version END
         FROM session_index AS i, current_codex_home AS h
         WHERE i.provider='codex' AND i.session_id=?1 AND h.home IS NOT NULL AND i.codex_home IS NOT h.home
           AND d.provider=i.provider AND d.session_id=i.session_id AND d.origin='native'
           AND d.derived_title_is_local_opener IS NOT 1 AND i.title IS NOT NULL
           AND d.source_file_id IS i.source_file_id AND (i.source_file_id IS NULL OR i.source_file_id=?2)
           AND (i.source_file_id IS NULL OR (d.source_contents_id=i.source_contents_id AND d.source_updated_at=i.updated_at)
                OR (d.source_contents_id=?4 AND d.source_updated_at=?5))
           AND d.derived_title IN (i.title,omnis_compact_search_title(i.title))",
        params![selected.session.id, file.into_bytes(), recheck, contents.into_bytes(), selected.updated_at.map(|t| t.timestamp_millis())],
    ).map_err(database_error)?;
    Ok(())
}

fn remember_provider_name(
    connection: &Connection,
    prior: &IndexedSession,
    selected: &IndexedSession,
) -> Result<bool> {
    let (Some(name), Some(old), Some(new)) = (
        &prior.title,
        prior.source_revision,
        selected.source_revision,
    ) else {
        return Ok(false);
    };
    if old.file.is_none()
        || old.file != new.file
        || old.contents.is_none()
        || new.contents.is_none()
    {
        return Ok(false);
    }
    let inserted = connection.execute(
        "INSERT INTO codex_pending_catalog_titles
            (trajectory_id, source_file_id, source_contents_id, title, evidence)
         SELECT id, source_file_id, source_contents_id, derived_title, 'provider_name' FROM session_trajectories
         WHERE provider='codex' AND session_id=?1 AND origin='native' AND source_file_id=?2
           AND ((source_contents_id=?3 AND source_updated_at=?4) OR (source_contents_id=?5 AND source_updated_at=?6))
           AND derived_title_is_verified AND derived_title_is_local_opener IS NOT 1
           AND derived_title=omnis_compact_search_title(?7)
         ON CONFLICT (trajectory_id) DO UPDATE SET source_file_id=excluded.source_file_id,
           source_contents_id=excluded.source_contents_id, title=excluded.title, evidence='provider_name'
         WHERE evidence<>'provider_name' OR source_file_id<>excluded.source_file_id
            OR source_contents_id<>excluded.source_contents_id OR title<>excluded.title",
        params![prior.session.id, old.file.map(Uuid::into_bytes), old.contents.map(Uuid::into_bytes),
           prior.updated_at.map(|t| t.timestamp_millis()), new.contents.map(Uuid::into_bytes),
           selected.updated_at.map(|t| t.timestamp_millis()), name]
    ).map_err(database_error)?;
    Ok(inserted != 0)
}

/// Moves a pending name check to the new contents only if the writer retained the verified title.
fn carry_pending_catalog_title(
    connection: &Connection,
    session: &SessionRef,
    prior: SourceRevision,
    current: SourceRevision,
) -> Result<()> {
    connection.execute(
        "UPDATE codex_pending_catalog_titles AS p SET source_contents_id=?4
         FROM session_trajectories AS t WHERE t.provider='codex' AND t.session_id=?1 AND t.origin='native'
           AND p.trajectory_id=t.id AND p.evidence<>'uncataloged' AND p.source_file_id=?2 AND p.source_contents_id=?3
           AND t.source_file_id=?2 AND t.source_contents_id=?4 AND t.derived_title=p.title
           AND t.derived_title_is_verified AND t.derived_title_is_local_opener IS NOT 1",
        params![session.id, current.file.map(Uuid::into_bytes), prior.contents.map(Uuid::into_bytes),
            current.contents.map(Uuid::into_bytes)]
    ).map_err(database_error)?;
    Ok(())
}

fn resolve_pending_catalog_title(
    connection: &Connection,
    selected: &IndexedSession,
    recheck: bool,
) -> Result<()> {
    let revision = selected.source_revision.unwrap_or_default();
    if selected.title.is_none() {
        connection.execute(
            "UPDATE session_trajectories AS t
             SET derived_title=CASE WHEN p.evidence='unattributed' THEN t.derived_title END,
                 derived_title_is_verified=(p.evidence='unattributed' AND t.derived_title_is_verified),
                 derived_title_is_local_opener=CASE WHEN p.evidence='unattributed' THEN t.derived_title_is_local_opener END,
                 document_version=CASE WHEN ?4 THEN 0 ELSE document_version END
             FROM codex_pending_catalog_titles AS p WHERE t.provider='codex' AND t.session_id=?1 AND t.origin='native'
               AND p.trajectory_id=t.id AND t.source_file_id=?2 AND p.source_file_id=t.source_file_id
               AND p.source_contents_id=t.source_contents_id AND (p.evidence<>'uncataloged' OR t.source_contents_id=?3)
               AND p.title=t.derived_title AND t.derived_title_is_local_opener IS NOT 1",
            params![selected.session.id, revision.file.map(Uuid::into_bytes), revision.contents.map(Uuid::into_bytes), recheck],
        ).map_err(database_error)?;
    }
    connection.execute(
        "DELETE FROM codex_pending_catalog_titles WHERE trajectory_id=(SELECT id FROM session_trajectories
         WHERE provider='codex' AND session_id=?1)", [&selected.session.id],
    ).map_err(database_error)?;
    Ok(())
}

fn pending_codex_catalog_titles(connection: &Connection) -> Result<HashMap<String, bool>> {
    let mut query = connection
        .prepare_cached(
            "SELECT t.session_id, t.source_file_id IS p.source_file_id AND t.source_contents_id IS p.source_contents_id
                  AND t.derived_title IS p.title AND t.derived_title_is_local_opener IS NOT 1
             FROM codex_pending_catalog_titles p JOIN session_trajectories t ON t.id=p.trajectory_id",
        )
        .map_err(database_error)?;
    query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(database_error)?
        .collect::<std::result::Result<_, _>>()
        .map_err(database_error)
}

fn uncataloged_trajectory_title(
    connection: &Connection,
    selected: &IndexedSession,
) -> Result<Option<(i64, String)>> {
    let Some(revision) = selected.source_revision else {
        return Ok(None);
    };
    connection
        .query_row(
            "SELECT id, derived_title FROM session_trajectories
         WHERE provider='codex' AND session_id=?1 AND origin='native'
           AND source_updated_at=?2 AND source_file_id=?3 AND source_contents_id=?4
           AND length(derived_title)>0 AND derived_title_is_local_opener IS NOT 1",
            params![
                selected.session.id,
                selected.updated_at.map(|t| t.timestamp_millis()),
                revision.file.map(Uuid::into_bytes),
                revision.contents.map(Uuid::into_bytes)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(database_error)
}

fn withdraw_catalog_name(
    connection: &Connection,
    old: &IndexedSession,
    recheck: bool,
) -> Result<()> {
    let Some(ref name) = old.title else {
        return Ok(());
    };
    withdraw_trajectory_name(connection, old, name, recheck)
}

fn withdraw_trajectory_name(
    connection: &Connection,
    old: &IndexedSession,
    name: &str,
    recheck: bool,
) -> Result<()> {
    let Some(revision) = old.source_revision else {
        return Ok(());
    };
    connection
        .execute(
            "UPDATE session_trajectories SET derived_title = NULL, derived_title_is_verified = 0, derived_title_is_local_opener = NULL,
             document_version = CASE WHEN ?6 THEN 0 ELSE document_version END
         WHERE provider = 'codex' AND session_id = ?1 AND origin = 'native'
           AND source_updated_at = ?2 AND source_file_id = ?3 AND source_contents_id = ?4
           AND derived_title IN (?5, ?7)",
            params![
                old.session.id,
                old.updated_at.map(|t| t.timestamp_millis()),
                revision.file.map(Uuid::into_bytes),
                revision.contents.map(Uuid::into_bytes),
                omnis_core::compact_search_title(name),
                recheck,
                name
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn reconcile_named_trajectories(
    connection: &Connection,
    changed_search_rows: Option<&[IndexedSession]>,
) -> Result<()> {
    const UPDATE_NAMES: &str = "UPDATE session_trajectories AS t
         SET derived_title = omnis_compact_search_title(i.title), derived_title_is_verified = 1, derived_title_is_local_opener = 0
         FROM scoped_session_catalog AS i WHERE i.provider = 'codex' AND i.name_observable AND t.origin = 'native'
           AND t.provider = i.provider AND t.session_id = i.session_id
           AND t.source_updated_at = i.updated_at AND t.source_file_id = i.source_file_id
           AND t.source_contents_id = i.source_contents_id AND i.title IS NOT NULL
           AND (i.title IS NOT t.derived_title OR NOT t.derived_title_is_verified OR t.derived_title_is_local_opener IS NOT 0)
           AND omnis_compact_search_title(i.title) IS NOT NULL
           AND (omnis_compact_search_title(i.title) IS NOT t.derived_title OR NOT t.derived_title_is_verified OR t.derived_title_is_local_opener IS NOT 0)";
    if let Some(rows) = changed_search_rows {
        let sql = format!("{UPDATE_NAMES} AND i.session_id = ?1");
        let mut statement = connection.prepare_cached(&sql).map_err(database_error)?;
        for row in rows.iter().filter(|row| row.title.is_some()) {
            statement
                .execute([&row.session.id])
                .map_err(database_error)?;
        }
    } else {
        connection
            .execute(UPDATE_NAMES, [])
            .map_err(database_error)?;
    }
    Ok(())
}

fn provider_index_matches(
    store: &Store,
    provider: Provider,
    sessions: &[IndexedSession],
) -> Result<bool> {
    if store.session_index_refreshed_at(provider)?.is_none() {
        return Ok(false);
    }
    let mut existing = store.indexed_sessions_for_provider(provider)?;
    let mut incoming = sessions.iter().collect::<Vec<_>>();
    existing.sort_by(|left, right| left.session.id.cmp(&right.session.id));
    incoming.sort_by(|left, right| left.session.id.cmp(&right.session.id));
    Ok(existing.len() == incoming.len()
        && existing
            .iter()
            .zip(incoming)
            .all(|(left, right)| left == right))
}

fn index_check_key(connection: &Connection, provider: Provider) -> Result<Option<String>> {
    if provider != Provider::Codex {
        return Ok(Some(provider.to_string()));
    }
    connection
        .query_row(
            "SELECT ?1 || home FROM current_codex_home",
            [CODEX_SCAN_KEY_PREFIX],
            |row| row.get(0),
        )
        .map_err(database_error)
}

fn record_complete_catalog(
    tx: &Transaction<'_>,
    key: &str,
    indexed_at: i64,
    rows_changed: bool,
) -> Result<()> {
    tx.execute(
        "INSERT INTO session_index_state (provider,indexed_at) VALUES (?1,?2)
         ON CONFLICT(provider) DO UPDATE SET indexed_at=excluded.indexed_at WHERE ?3",
        params![key, indexed_at, rows_changed],
    )
    .map_err(database_error)?;
    Ok(())
}

fn record_session_check(connection: &Connection, key: &str, checked_at: i64) -> Result<()> {
    connection
        .execute(
            "INSERT INTO session_index_checks (provider, checked_at)
             VALUES (?1, ?2)
             ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at",
            params![key, checked_at],
        )
        .map_err(database_error)?;
    Ok(())
}

fn record_unchanged_provider_refresh(store: &Store, provider: Provider) -> Result<()> {
    let mut connection = store.connection.borrow_mut();
    let transaction = immediate_transaction(&mut connection)?;
    let provider_name = provider.to_string();
    if let Some(key) = index_check_key(&transaction, provider)? {
        record_session_check(&transaction, &key, now_timestamp())?;
    }
    prune_stale_native_trajectories(&transaction, &provider_name)?;
    transaction.commit().map_err(database_error)
}

fn prune_stale_native_trajectories(transaction: &Transaction<'_>, provider: &str) -> Result<()> {
    // A direct Codex read can save a document without identifying its home in the catalog.
    if provider == "codex" {
        return Ok(());
    }
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

fn migrate_inferred_document_homes(tx: &Transaction<'_>) -> Result<()> {
    let (legacy, missing_count) = tx
        .query_row(
            "SELECT count(*) > 0 AND count(*) FILTER (WHERE name='verification')=0,
                count(*) > 0 AND count(*) FILTER (WHERE name='declared_steps')=0
         FROM pragma_table_info('native_search_document_homes')",
            [],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )
        .map_err(database_error)?;
    if legacy {
        tx.execute_batch(
            "ALTER TABLE native_search_document_homes ADD COLUMN verification INTEGER NOT NULL DEFAULT 0 CHECK(verification IN (-1,0,1));"
        ).map_err(database_error)?;
    }
    if missing_count {
        tx.execute_batch(
            "ALTER TABLE native_search_document_homes ADD COLUMN declared_steps INTEGER CHECK(declared_steps >= 0);
             UPDATE native_search_document_homes AS origin SET declared_steps = catalog.event_count
             FROM session_trajectories AS doc JOIN session_index AS catalog USING(provider,session_id)
             JOIN native_catalog_homes AS native USING(provider,session_id)
             WHERE doc.provider = 'antigravity-desktop' AND doc.origin = 'native' AND doc.source_complete
               AND doc.id = origin.trajectory_id AND origin.verification = 1 AND native.verified
               AND origin.source_home = native.home AND origin.source_file = doc.source_file_id
               AND doc.source_file_id = catalog.source_file_id AND doc.source_contents_id = catalog.source_contents_id
               AND catalog.event_count > 0;"
        ).map_err(database_error)?;
    }
    let inferred = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name='remember_rebound_document_home')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    if inferred {
        // The old trigger copied a catalog home without observing the document in that home.
        // Keep unchallenged rows for offline use. A native observation can settle their ownership.
        tx.execute_batch(
            "UPDATE native_search_document_homes AS origin SET verification=-1
             WHERE verification=0 AND EXISTS (
                SELECT 1 FROM session_trajectories AS doc
                LEFT JOIN native_catalog_homes AS catalog USING(provider,session_id)
                LEFT JOIN trajectory_index_failures AS failed USING(provider,session_id)
                WHERE doc.id=origin.trajectory_id AND (catalog.home IS NOT origin.source_home OR NOT coalesce(catalog.verified,0)
                    OR (failed.source_file_id IS NOT NULL AND failed.source_file_id IS NOT origin.source_file)))"
        ).map_err(database_error)?;
    }
    Ok(())
}

fn migrate_ide_home_triggers(transaction: &Transaction<'_>) -> Result<()> {
    let old_home_triggers = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='detach_changed_legacy_locator' AND instr(sql,'antigravity-ide')=0)",
        [], |row| row.get::<_, bool>(0),
    ).map_err(database_error)?;
    if old_home_triggers {
        transaction
            .execute_batch(
                "DROP TRIGGER IF EXISTS detach_changed_legacy_locator;
             DROP TRIGGER IF EXISTS detach_removed_legacy_locator;
             DROP TRIGGER IF EXISTS forget_changed_catalog_home;",
            )
            .map_err(database_error)?;
    }
    Ok(())
}

fn initialize_discovery_schema(transaction: &Transaction<'_>) -> Result<()> {
    migrate_inferred_document_homes(transaction)?;
    migrate_ide_home_triggers(transaction)?;
    transaction
        .execute_batch(
            "
                CREATE TABLE IF NOT EXISTS native_catalog_homes (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    home BLOB NOT NULL,
                    verified INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (provider, session_id),
                    FOREIGN KEY (provider, session_id) REFERENCES session_index (provider, session_id)
                        ON DELETE CASCADE
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS native_search_document_homes (
                    trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                    source_file BLOB NOT NULL,
                    source_home BLOB NOT NULL,
                    verification INTEGER NOT NULL DEFAULT 0 CHECK(verification IN (-1, 0, 1)),
                    declared_steps INTEGER CHECK(declared_steps >= 0)
                );

                CREATE INDEX IF NOT EXISTS unverified_document_homes ON native_search_document_homes(trajectory_id) WHERE verification < 1;
                DROP TRIGGER IF EXISTS remember_rebound_document_home;

                CREATE TABLE IF NOT EXISTS detached_native_documents (
                    trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE
                );

                CREATE TRIGGER IF NOT EXISTS detach_changed_legacy_locator
                AFTER UPDATE ON session_index
                WHEN OLD.provider IN ('antigravity', 'antigravity-desktop', 'antigravity-ide')
                 AND (OLD.title, OLD.project_path, OLD.git_branch, OLD.created_at, OLD.updated_at, OLD.event_count)
                     IS NOT (NEW.title, NEW.project_path, NEW.git_branch, NEW.created_at, NEW.updated_at, NEW.event_count)
                BEGIN
                    INSERT INTO detached_native_documents(trajectory_id)
                    SELECT id FROM session_trajectories AS doc
                    WHERE provider = OLD.provider AND session_id = OLD.session_id AND origin = 'native'
                      AND NOT EXISTS (SELECT 1 FROM native_search_document_homes WHERE trajectory_id = doc.id AND source_file = doc.source_file_id)
                    ON CONFLICT(trajectory_id) DO NOTHING;
                END;

                CREATE TRIGGER IF NOT EXISTS detach_removed_legacy_locator
                BEFORE DELETE ON session_index
                WHEN OLD.provider IN ('antigravity', 'antigravity-desktop', 'antigravity-ide')
                BEGIN
                    INSERT INTO detached_native_documents(trajectory_id)
                    SELECT id FROM session_trajectories AS doc
                    WHERE provider = OLD.provider AND session_id = OLD.session_id AND origin = 'native'
                      AND NOT EXISTS (SELECT 1 FROM native_search_document_homes WHERE trajectory_id = doc.id AND source_file = doc.source_file_id)
                    ON CONFLICT(trajectory_id) DO NOTHING;
                END;

                CREATE TRIGGER IF NOT EXISTS forget_changed_catalog_home
                AFTER UPDATE ON session_index
                WHEN NEW.provider IN ('antigravity', 'antigravity-desktop', 'antigravity-ide')
                 AND (OLD.title, OLD.project_path, OLD.git_branch, OLD.created_at, OLD.updated_at, OLD.event_count)
                     IS NOT (NEW.title, NEW.project_path, NEW.git_branch, NEW.created_at, NEW.updated_at, NEW.event_count)
                BEGIN
                    UPDATE native_catalog_homes SET verified = 0
                    WHERE provider = NEW.provider AND session_id = NEW.session_id AND verified = 1;
                END;

                CREATE TABLE IF NOT EXISTS session_index_reader_checks (
                    provider TEXT NOT NULL,
                    reader_version INTEGER NOT NULL,
                    source_home BLOB NOT NULL,
                    checked_at INTEGER NOT NULL,
                    attempt_id BLOB NOT NULL,
                    PRIMARY KEY (provider, reader_version)
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS session_index_discovery_results (
                    provider TEXT NOT NULL,
                    reader_version INTEGER NOT NULL,
                    attempt_id BLOB NOT NULL,
                    succeeded INTEGER NOT NULL CHECK (succeeded IN (0, 1)),
                    PRIMARY KEY (provider, reader_version),
                    FOREIGN KEY (provider, reader_version)
                        REFERENCES session_index_reader_checks(provider, reader_version) ON DELETE CASCADE
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS session_workspace_catalogs (
                    provider TEXT NOT NULL,
                    workspace BLOB NOT NULL,
                    source_home BLOB NOT NULL,
                    fingerprint BLOB NOT NULL,
                    generation INTEGER NOT NULL,
                    PRIMARY KEY (provider, workspace)
                ) WITHOUT ROWID;

                CREATE INDEX IF NOT EXISTS recent_workspace_catalog ON session_workspace_catalogs(provider, generation DESC);

                CREATE TABLE IF NOT EXISTS desktop_discovery_snapshots (
                    reader_version INTEGER NOT NULL,
                    source_home BLOB NOT NULL,
                    session_ids TEXT NOT NULL,
                    PRIMARY KEY (reader_version, source_home)
                ) WITHOUT ROWID;

                CREATE TRIGGER IF NOT EXISTS expire_rebound_catalog_home
                AFTER UPDATE OF home ON native_catalog_homes WHEN OLD.home IS NOT NEW.home
                BEGIN
                    DELETE FROM session_index_reader_checks WHERE provider = OLD.provider AND source_home = OLD.home;
                END;
        ",
        )
        .map_err(database_error)
}

fn ensure_session_index_columns(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_index_checks (
                provider TEXT PRIMARY KEY,
                checked_at INTEGER NOT NULL
            ) WITHOUT ROWID;",
        )
        .map_err(database_error)?;
    let mut statement = transaction
        .prepare("PRAGMA table_info(session_index)")
        .map_err(database_error)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(database_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(database_error)?;
    drop(statement);
    if !columns
        .iter()
        .any(|column| column == "updated_at_approximate")
    {
        transaction.execute_batch(
            "ALTER TABLE session_index ADD COLUMN updated_at_approximate INTEGER NOT NULL DEFAULT 0
             CHECK (updated_at_approximate IN (0, 1));"
        ).map_err(database_error)?;
    }
    if !columns.iter().any(|name| name == "codex_home") {
        transaction
            .execute_batch("ALTER TABLE session_index ADD COLUMN codex_home TEXT;")
            .map_err(database_error)?;
    }
    if !columns.iter().any(|name| name == "codex_legacy_name") {
        transaction
            .execute_batch(
                "ALTER TABLE session_index ADD COLUMN codex_legacy_name INTEGER NOT NULL DEFAULT 0
             CHECK (codex_legacy_name IN (0, 1));",
            )
            .map_err(database_error)?;
    }
    for name in ["source_file_id", "source_contents_id"] {
        if !columns.iter().any(|column| column == name) {
            transaction
                .execute_batch(&format!(
                    "ALTER TABLE session_index ADD COLUMN {name} BLOB
                     CHECK ({name} IS NULL OR length({name}) = 16)"
                ))
                .map_err(database_error)?;
        }
    }
    let expire_previous_home = format!(
        "DELETE FROM session_index_state WHERE provider = '{CODEX_SCAN_KEY_PREFIX}' || OLD.codex_home;
         DELETE FROM session_index_checks WHERE provider = '{CODEX_SCAN_KEY_PREFIX}' || OLD.codex_home;"
    );
    transaction
        .execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS codex_session_reassigned AFTER UPDATE OF codex_home ON session_index
             WHEN OLD.provider = 'codex' AND OLD.codex_home IS NOT NULL AND OLD.codex_home IS NOT NEW.codex_home
             BEGIN {expire_previous_home} END;
             CREATE TRIGGER IF NOT EXISTS codex_session_evicted AFTER DELETE ON session_index
             WHEN OLD.provider = 'codex' AND OLD.codex_home IS NOT NULL
             BEGIN {expire_previous_home} END;"
        ))
        .map_err(database_error)
}

fn ensure_trajectory_source_revision_columns(transaction: &Transaction<'_>) -> Result<()> {
    for (table, column) in [
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
    Ok(())
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

fn optional_uuid(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<Uuid>> {
    row.get::<_, Option<[u8; 16]>>(column)
        .map(|id| id.map(Uuid::from_bytes))
}

fn ensure_trajectory_source_columns(transaction: &Transaction<'_>) -> Result<()> {
    // Existing rows keep a NULL remainder until their file is next read.
    for table in ["session_trajectories", "trajectory_index_failures"] {
        let (has_remainder, has_summary) = transaction
            .query_row(
                "SELECT count(*) FILTER (WHERE name = 'source_updated_submillis') > 0,
                        count(*) FILTER (WHERE name = 'native_summary') > 0
                 FROM pragma_table_info(?1)",
                [table],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
            )
            .map_err(database_error)?;
        if !has_remainder {
            transaction
                .execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN source_updated_submillis INTEGER
                 CHECK (source_updated_submillis BETWEEN 0 AND 999999)"
                ))
                .map_err(database_error)?;
        }
        if !has_summary {
            transaction
                .execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN native_summary INTEGER NOT NULL DEFAULT 0
                     CHECK (native_summary IN (0, 1))"
                ))
                .map_err(database_error)?;
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
            CREATE TABLE IF NOT EXISTS codex_pending_catalog_titles (
                trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                source_file_id BLOB NOT NULL,
                source_contents_id BLOB NOT NULL,
                title TEXT NOT NULL,
                evidence TEXT NOT NULL DEFAULT 'uncataloged' CHECK (evidence IN ('uncataloged', 'provider_name', 'unattributed'))
            );

            ",
        )
        .map_err(database_error)?;
    ensure_trajectory_coverage_columns(transaction)?;
    initialize_legacy_codex_v5_projections(transaction)?;
    ensure_trajectory_source_revision_columns(transaction)?;
    ensure_trajectory_source_columns(transaction)?;
    codex_workspace::initialize(transaction)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS legacy_codex_name_retries(
             trajectory_id INTEGER PRIMARY KEY REFERENCES legacy_codex_title_proofs(trajectory_id) ON DELETE CASCADE,
             codex_home TEXT NOT NULL);"
    ).map_err(database_error)?;
    ensure_trajectory_title_provenance(transaction)?;
    codex_title_recheck::schedule_tool_catalog_title_rechecks(transaction)?;
    migrate_pending_catalog_names(transaction)?;
    initialize_trajectory_chunk_schema(transaction)?;
    migrate_legacy_codex_titles(transaction)
}

/// Older catalogs could lose the source of a verified title during an append.
fn migrate_pending_catalog_names(transaction: &Transaction<'_>) -> Result<()> {
    let has_evidence: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('codex_pending_catalog_titles') WHERE name='evidence')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if !has_evidence {
        transaction.execute_batch(
            "ALTER TABLE codex_pending_catalog_titles ADD COLUMN evidence TEXT NOT NULL DEFAULT 'uncataloged'
                  CHECK (evidence IN ('uncataloged','provider_name','unattributed'));
             INSERT OR IGNORE INTO codex_pending_catalog_titles (trajectory_id,source_file_id,source_contents_id,title,evidence)
             SELECT t.id,t.source_file_id,t.source_contents_id,t.derived_title,'unattributed'
             FROM session_trajectories t JOIN session_index i ON t.provider=i.provider AND t.session_id=i.session_id
             WHERE t.provider='codex' AND t.origin='native' AND t.document_version>=5 AND length(t.derived_title)>0
               AND t.derived_title_is_verified AND t.derived_title_is_local_opener IS 0
               AND i.title IS NULL AND i.updated_at=t.source_updated_at AND i.source_file_id=t.source_file_id
               AND i.source_contents_id=t.source_contents_id;",
        ).map_err(database_error)?;
    }
    Ok(())
}

fn initialize_legacy_codex_v5_projections(transaction: &Transaction<'_>) -> Result<()> {
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_codex_v5_projections')",
            [], |row| row.get(0),
        )
        .map_err(database_error)?;
    if exists {
        return Ok(());
    }
    transaction
        .execute_batch(
            "CREATE TABLE legacy_codex_v5_projections (
            trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
            source_updated_at INTEGER NOT NULL,
            content_hash BLOB NOT NULL CHECK (length(content_hash) = 32)
         );",
        )
        .map_err(database_error)?;
    let lacks_revisions: bool = transaction
        .query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM pragma_table_info('session_trajectories')
                                WHERE name IN ('source_file_id', 'source_contents_id'))",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if lacks_revisions {
        transaction.execute_batch(
            "INSERT INTO legacy_codex_v5_projections (trajectory_id, source_updated_at, content_hash)
             SELECT id, source_updated_at, content_hash FROM session_trajectories
             WHERE provider = 'codex' AND origin = 'native' AND document_version = 5
               AND source_complete = 1 AND indexed_byte_count > 0
               AND typeof(content_hash) = 'blob' AND length(content_hash) = 32;",
        ).map_err(database_error)?;
    }
    Ok(())
}

fn ensure_trajectory_title_provenance(transaction: &Transaction<'_>) -> Result<()> {
    let (verified, local): (bool, bool) = transaction
        .query_row(
            "SELECT count(*) FILTER (WHERE name = 'derived_title_is_verified'),
                count(*) FILTER (WHERE name = 'derived_title_is_local_opener')
         FROM pragma_table_info('session_trajectories')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(database_error)?;
    if verified && local {
        return Ok(());
    }
    if !verified {
        // Older documents could retain a withdrawn first prompt after indexing an incomplete leaf.
        // Keep titles confirmed by the catalog or a full read. Recheck the remaining candidates.
        transaction.execute_batch(
            "ALTER TABLE session_trajectories ADD COLUMN derived_title_is_verified INTEGER NOT NULL
                 DEFAULT 0 CHECK (derived_title_is_verified IN (0, 1));
             UPDATE session_trajectories AS doc SET derived_title_is_verified = 1
             WHERE doc.provider = 'codex' AND doc.origin = 'native' AND length(doc.derived_title) > 0
               AND ((doc.source_file_id IS NOT NULL AND doc.source_complete AND doc.truncation_strategy IN (
                       'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'))
                    OR EXISTS (SELECT 1 FROM session_index AS catalog
                               WHERE catalog.provider = doc.provider AND catalog.session_id = doc.session_id
                                 AND (catalog.source_file_id IS NULL OR catalog.source_file_id IS doc.source_file_id)
                                 AND (catalog.title = doc.derived_title
                                      OR omnis_compact_search_title(catalog.title) = doc.derived_title)));
             UPDATE session_trajectories SET document_version = CASE WHEN source_file_id IS NULL THEN 0 ELSE 4 END
             WHERE provider = 'codex' AND origin = 'native' AND document_version = 5
               AND truncation_strategy GLOB 'source_incomplete*' AND length(derived_title) > 0
               AND NOT derived_title_is_verified
               AND NOT EXISTS (SELECT 1 FROM legacy_codex_v5_projections AS old
                               WHERE old.trajectory_id = session_trajectories.id
                                 AND old.source_updated_at = session_trajectories.source_updated_at
                                 AND old.content_hash = session_trajectories.content_hash);"
        ).map_err(database_error)?;
    }
    if !local {
        transaction.execute_batch(
            "ALTER TABLE session_trajectories ADD COLUMN derived_title_is_local_opener INTEGER
                 CHECK (derived_title_is_local_opener IN (0, 1));
             UPDATE session_trajectories AS doc SET derived_title_is_local_opener = 0
             WHERE doc.provider = 'codex' AND doc.origin = 'native' AND doc.derived_title_is_verified
               AND doc.derived_title IS NOT NULL AND EXISTS (
                   SELECT 1 FROM session_index AS catalog
                   WHERE catalog.provider = doc.provider AND catalog.session_id = doc.session_id
                     AND (catalog.source_file_id IS NULL OR catalog.source_file_id IS doc.source_file_id)
                     AND (catalog.title = doc.derived_title OR omnis_compact_search_title(catalog.title) = doc.derived_title)
               );"
        ).map_err(database_error)?;
        codex_workspace::recheck_legacy_projections(transaction)?;
    }
    Ok(())
}

fn cached_index_failure(
    connection: &Connection,
    session: &SessionRef,
) -> Result<Option<TrajectoryIndexFailure>> {
    connection
        .query_row(
            "SELECT source_updated_at, document_version, source_updated_submillis, native_summary,
                    source_file_id, source_contents_id
             FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2",
            params![session.provider.to_string(), session.id],
            |row| trajectory_failure_from_row(row, 0),
        )
        .optional()
        .map_err(database_error)
}

fn clear_observed_failure(
    connection: &Connection,
    session: &SessionRef,
    failure: TrajectoryIndexFailure,
) -> Result<()> {
    update_observed_failure(
        connection,
        session,
        failure,
        "DELETE FROM trajectory_index_failures",
    )
}

fn update_observed_failure(
    connection: &Connection,
    session: &SessionRef,
    failure: TrajectoryIndexFailure,
    action: &'static str,
) -> Result<()> {
    connection
        .execute(
            &format!(
                "{action} WHERE provider = ?1 AND session_id = ?2
                 AND source_updated_at IS ?3 AND document_version = ?4
                 AND source_updated_submillis IS ?5 AND native_summary = ?6
                 AND source_file_id IS ?7 AND source_contents_id IS ?8"
            ),
            params![
                session.provider.to_string(),
                session.id,
                failure
                    .source_updated_at
                    .map(|time| time.timestamp_millis()),
                failure.document_version,
                failure
                    .source_updated_at
                    .filter(|_| failure.source_time_precise)
                    .map(|time| source_submillis(session.provider, time)),
                failure.native_summary,
                failure.source_file.map(Uuid::into_bytes),
                failure.source_contents.map(Uuid::into_bytes)
            ],
        )
        .map_err(database_error)?;
    Ok(())
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
    ensure_bundle_protection(transaction)
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
    Ok(())
}

/// Legacy writers could retain a guessed title even after a full read with no user message.
fn migrate_legacy_codex_titles(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_trajectory_migrations (
                 name TEXT PRIMARY KEY
             ) WITHOUT ROWID;",
        )
        .map_err(database_error)?;
    let mark = |name: &str| {
        transaction
            .execute(
                "INSERT OR IGNORE INTO session_trajectory_migrations (name) VALUES (?1)",
                [name],
            )
            .map_err(database_error)
    };
    // Keep the old version only when the next read can verify the saved projection.
    let legacy_version = "CASE WHEN doc.source_complete AND doc.source_file_id IS NULL
        AND doc.source_contents_id IS NULL AND doc.missing_ancestor_id IS NULL
        AND typeof(doc.content_hash) = 'blob' AND length(doc.content_hash) = 32
        AND (doc.document_version = 4 OR (doc.document_version = 6 AND doc.source_updated_submillis IS NOT NULL)
             OR (doc.document_version = 5 AND EXISTS (
            SELECT 1 FROM legacy_codex_v5_projections AS old
            WHERE old.trajectory_id = doc.id AND old.source_updated_at = doc.source_updated_at
              AND old.content_hash = doc.content_hash))) THEN doc.document_version ELSE 0 END";
    if mark("recheck_v5_codex_coverage")? != 0 {
        // The v5 reader skipped malformed records. Keep the full source receipt for its next read.
        transaction
            .execute(
                &format!("UPDATE session_trajectories AS doc SET document_version = {legacy_version},
                    complete = 0, truncation_strategy = 'source_incomplete'
                 WHERE provider = 'codex' AND origin = 'native' AND document_version = 5 AND complete"),
                [],
            )
            .map_err(database_error)?;
    }
    if mark("reverify_legacy_codex_titles")? == 0 {
        return Ok(());
    }
    // The earlier repair erased some titles without scheduling a new read of those sources.
    let previous_repair = mark("codex_title_authority")? == 0;
    transaction
        .execute_batch(
            "UPDATE session_trajectories AS doc SET (derived_title_is_verified, derived_title_is_local_opener) = (
             SELECT named OR local, CASE WHEN named THEN 0 WHEN local THEN 1 END FROM (SELECT EXISTS (
                 SELECT 1 FROM session_index AS catalog
                 WHERE catalog.provider = doc.provider AND catalog.session_id = doc.session_id
                   AND catalog.source_file_id IS doc.source_file_id
                   AND catalog.source_contents_id IS doc.source_contents_id
                   AND (catalog.title = doc.derived_title
                        OR omnis_compact_search_title(catalog.title) = doc.derived_title)) AS named,
                 (doc.document_version >= 6 AND doc.derived_title_is_verified AND doc.derived_title_is_local_opener IS 1
                  AND typeof(doc.source_file_id) = 'blob' AND length(doc.source_file_id) = 16
                  AND typeof(doc.source_contents_id) = 'blob' AND length(doc.source_contents_id) = 16) AS local))
         WHERE doc.provider = 'codex' AND doc.origin = 'native' AND doc.derived_title IS NOT NULL;",
        )
        .map_err(database_error)?;
    transaction
        .execute(
            &format!("UPDATE session_trajectories AS doc SET derived_title = NULL, derived_title_is_verified = 0, derived_title_is_local_opener = NULL,
               document_version = {legacy_version}
             WHERE provider = 'codex' AND origin = 'native' AND NOT derived_title_is_verified
               AND (derived_title IS NOT NULL OR (?1 AND (source_complete = 0 OR truncation_strategy NOT IN (
                   'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'
               ))))"),
            [previous_repair],
        )
        .map_err(database_error)?;
    Ok(())
}

/// Provider of the session a stored bundle was made from, read from the bundle JSON.
const BUNDLE_SOURCE_PROVIDER_SQL: &str = "json_extract(
    CASE WHEN json_valid(bundle_json) THEN bundle_json END, '$.snapshot.session.provider')";
/// ID of the session a stored bundle was made from, read from the bundle JSON.
const BUNDLE_SOURCE_SESSION_ID_SQL: &str = "json_extract(
    CASE WHEN json_valid(bundle_json) THEN bundle_json END, '$.snapshot.session.id')";

/// Indexes the source session so a new trajectory can check for a bundle without reading its JSON.
fn initialize_bundle_source_index(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS bundle_source_session ON bundles (
                 {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
             );"
        ))
        .map_err(database_error)
}

/// Protects existing rows on upgrade and maintains protection when any compatible writer saves
/// a bundle or a trajectory. Check every trigger so reinstalling a missing one also repairs rows.
fn ensure_bundle_protection(transaction: &Transaction<'_>) -> Result<()> {
    let installed = transaction
        .query_row(
            "SELECT count(*) = 4 FROM sqlite_schema WHERE type = 'trigger' AND name IN (
                 'protect_trajectory_after_insert', 'protect_trajectory_after_update',
                 'protect_bundle_source_after_insert', 'protect_bundle_source_after_update'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?;
    if installed {
        return Ok(());
    }

    // Unary plus removes the column affinity so SQLite can look up both JSON expressions by index.
    let trajectory = format!(
        "WHEN new.protected_by_bundle = 0 AND (
             new.origin = 'imported_bundle' OR EXISTS (
                 SELECT 1 FROM bundles WHERE {BUNDLE_SOURCE_PROVIDER_SQL} = +new.provider
                   AND {BUNDLE_SOURCE_SESSION_ID_SQL} = +new.session_id
             )
         ) BEGIN
             UPDATE session_trajectories SET protected_by_bundle = 1 WHERE id = new.id;
         END;"
    );
    let bundle = format!(
        "BEGIN
             UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE protected_by_bundle = 0 AND (provider, session_id) = (
                 SELECT {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
                 FROM (SELECT new.bundle_json AS bundle_json)
             );
         END;"
    );
    transaction
        .execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS protect_trajectory_after_insert
             AFTER INSERT ON session_trajectories {trajectory}
             CREATE TRIGGER IF NOT EXISTS protect_trajectory_after_update
             AFTER UPDATE OF origin, provider, session_id, protected_by_bundle ON session_trajectories
             {trajectory}
             CREATE TRIGGER IF NOT EXISTS protect_bundle_source_after_insert
             AFTER INSERT ON bundles {bundle}
             CREATE TRIGGER IF NOT EXISTS protect_bundle_source_after_update
             AFTER UPDATE OF bundle_json ON bundles {bundle}

             UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE protected_by_bundle = 0 AND (
                 origin = 'imported_bundle' OR (provider, session_id) IN (
                     SELECT {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
                     FROM bundles
                 )
             );"
        ))
        .map_err(database_error)
}

fn trajectory_failure_from_row(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<TrajectoryIndexFailure> {
    let remainder = row.get::<_, Option<u32>>(column + 2)?;
    let source_updated_at = row
        .get::<_, Option<i64>>(column)?
        .map(|millis| source_time_from_db(millis, remainder))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(error))
        })?;
    Ok(TrajectoryIndexFailure {
        source_updated_at,
        source_time_precise: remainder.is_some(),
        document_version: row.get(column + 1)?,
        native_summary: row.get(column + 3)?,
        source_file: optional_uuid(row, column + 4)?,
        source_contents: optional_uuid(row, column + 5)?,
    })
}

fn native_failure_receipt(
    connection: &Connection,
    session: &SessionRef,
) -> Result<Option<[u8; 32]>> {
    connection.prepare_cached(
        "SELECT json_array(failed_at, document_version, source_updated_at, source_updated_submillis,
                           native_summary, hex(source_file_id), hex(source_contents_id))
         FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2",
    ).map_err(database_error)?
        .query_row(params![session.provider.to_string(), session.id], |row| Ok(Sha256::digest(row.get::<_, String>(0)?).into()))
        .optional().map_err(database_error)
}

fn clear_observed_native_failure(
    transaction: &Transaction<'_>,
    write: &PreparedTrajectoryWrite<'_, '_>,
) -> Result<()> {
    if TrajectorySourceGuard::uses_catalog_home(write.session.provider)
        && write.document.origin != SessionTrajectoryOrigin::ImportedBundle
        && let TrajectorySourceGuard::ObservedDocument(observed) = write.guard
        && observed.failure.is_some()
        && native_failure_receipt(transaction, write.session)? == observed.failure
    {
        transaction
            .prepare_cached(
                "DELETE FROM trajectory_index_failures AS failed
                 WHERE provider = ?10 AND session_id = ?1 AND document_version <= ?2
                 AND EXISTS (
                     SELECT 1 FROM session_trajectories
                     WHERE provider = ?10 AND session_id = ?1
                       AND origin = 'native' AND document_version = ?2 AND native_summary = ?3
                       AND source_file_id IS ?4 AND source_contents_id IS ?5
                       AND missing_ancestor_id IS ?6
                       AND ((content_hash = ?7 AND source_complete = ?8 AND provider_limited = ?9)
                            OR (?4 IS NOT NULL AND ?5 IS NOT NULL
                                AND (source_complete, provider_limited) > (?8, ?9)
                                AND (failed.native_summary <> ?3 OR (
                                    failed.source_file_id IS NOT NULL AND failed.source_contents_id IS NOT NULL
                                    AND (failed.source_file_id <> ?4 OR failed.source_contents_id <> ?5)))))
                 )",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    write.session.id,
                    write.document.document_version,
                    write.document.origin == SessionTrajectoryOrigin::NativeSummary,
                    write.document.source_revision.file.map(Uuid::into_bytes),
                    write
                        .document
                        .source_revision
                        .contents
                        .map(Uuid::into_bytes),
                    write
                        .document
                        .source_revision
                        .missing_ancestor
                        .map(Uuid::into_bytes),
                    write.content_hash,
                    write.source_complete,
                    write.document.provider_limited,
                    write.session.provider.to_string(),
                ])
            })
            .map_err(database_error)?;
    }
    Ok(())
}

fn protect_bundle_trajectory(transaction: &Transaction<'_>, session: &SessionRef) -> Result<()> {
    transaction
        .execute(
            "UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE provider = ?1 AND session_id = ?2 AND protected_by_bundle = 0",
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

             CREATE INDEX IF NOT EXISTS session_trajectory_legacy_text
                 ON session_trajectories(id) WHERE redacted_text <> '';

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

/// Moves legacy parent text into chunks. The partial index contains only parents still holding text.
fn chunk_text_kept_with_trajectories(transaction: &Transaction<'_>) -> Result<()> {
    let existing = {
        let mut statement = transaction
            .prepare(
                "SELECT id, redacted_text FROM session_trajectories INDEXED BY session_trajectory_legacy_text
                 WHERE redacted_text <> ''
                   AND NOT EXISTS (
                       SELECT 1 FROM session_trajectory_chunk_rows
                       WHERE trajectory_id = session_trajectories.id AND chunk_index < ?1
                   )",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([FIRST_PRIMARY_LEGACY_CHUNK], |row| {
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
            "UPDATE session_trajectories INDEXED BY session_trajectory_legacy_text
             SET redacted_text = '' WHERE redacted_text <> ''",
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
             WHERE trajectory_id IN (SELECT value FROM json_each(?1)) AND chunk_index < ?2",
            params![trajectory_ids, FIRST_PRIMARY_LEGACY_CHUNK],
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
    insert_chunks_into(
        transaction,
        documents,
        0,
        "INSERT INTO session_trajectory_chunk_rows(trajectory_id, chunk_index, stored_text) VALUES ",
    )
}

fn insert_chunks_into(
    transaction: &Transaction<'_>,
    documents: &[(i64, &str)],
    first_chunk_index: i64,
    insert_sql: &str,
) -> Result<()> {
    rebuild_full_text_index_after_unreadable_chunk(transaction)?;
    let (mut rows, mut texts) = (Vec::new(), Vec::new());
    for (trajectory_id, text) in documents {
        for (chunk_index, chunk) in utf8_chunks(text, TRAJECTORY_CHUNK_BYTE_LIMIT).enumerate() {
            let chunk_index =
                i64::try_from(chunk_index).map_err(|_| StoreError::InvalidSessionReference)?;
            rows.push((*trajectory_id, first_chunk_index + chunk_index));
            texts.push(chunk);
        }
    }
    let rows = rows
        .into_iter()
        .zip(encode_chunks(&texts))
        .map(|((trajectory_id, chunk_index), chunk)| (trajectory_id, chunk_index, chunk))
        .collect::<Vec<_>>();
    for group in rows.chunks(TRAJECTORY_CHUNK_INSERT_ROWS) {
        let mut sql = String::from(insert_sql);
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

fn prepare_indexed_session(
    connection: &Connection,
    upsert: bool,
) -> Result<rusqlite::CachedStatement<'_>> {
    const INSERT: &str = "
        INSERT INTO session_index (
            provider, session_id, title, project_path, git_branch, created_at, updated_at,
            updated_at_approximate, event_count, indexed_at, source_file_id, source_contents_id, codex_home
        ) SELECT ?1,?2,?3,CASE WHEN p.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NULL THEN ?4 ELSE w.project_path END,
                 CASE WHEN p.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NULL THEN ?5 ELSE w.git_branch END,?6,?7,?8,?9,?10,?11,?12,
                 CASE WHEN ?1='codex' THEN (SELECT home FROM current_codex_home) END
          FROM (SELECT 1) LEFT JOIN codex_indexed_workspaces w
            ON ?1='codex' AND w.source_file_id=?11 AND w.source_contents_id IS ?12
          LEFT JOIN codex_workspace_rechecks p ON ?1='codex' AND p.source_file_id=?11 AND p.source_contents_id=?12
            AND p.trajectory_id=(SELECT id FROM session_trajectories WHERE provider=?1 AND session_id=?2) WHERE 1";
    const ON_CONFLICT: &str = "ON CONFLICT (provider, session_id) DO UPDATE SET
        title = excluded.title,
        project_path = excluded.project_path,
        git_branch = excluded.git_branch,
        created_at = excluded.created_at,
        updated_at = excluded.updated_at,
        updated_at_approximate = excluded.updated_at_approximate,
        event_count = excluded.event_count,
        source_file_id = excluded.source_file_id,
        source_contents_id = excluded.source_contents_id,
        codex_home = excluded.codex_home,
        codex_legacy_name = 0,
        indexed_at = excluded.indexed_at
        WHERE excluded.provider <> 'antigravity'
           OR (session_index.title, session_index.project_path, session_index.git_branch,
               session_index.created_at, session_index.updated_at, session_index.updated_at_approximate,
               session_index.event_count, session_index.source_file_id, session_index.source_contents_id) IS NOT
              (excluded.title, excluded.project_path, excluded.git_branch, excluded.created_at,
               excluded.updated_at, excluded.updated_at_approximate, excluded.event_count, excluded.source_file_id, excluded.source_contents_id)";
    if upsert {
        connection.prepare_cached(&format!("{INSERT} {ON_CONFLICT}"))
    } else {
        connection.prepare_cached(INSERT)
    }
    .map_err(database_error)
}

fn write_indexed_session(connection: &Connection, session: &IndexedSession) -> Result<()> {
    validate_session_ref(&session.session)?;
    let mut statement = prepare_indexed_session(connection, true)?;
    write_indexed_session_row(
        &mut statement,
        &session.session.provider.to_string(),
        session,
        now_timestamp(),
    )
}

fn write_indexed_session_row(
    statement: &mut rusqlite::Statement<'_>,
    provider: &str,
    session: &IndexedSession,
    indexed_at: i64,
) -> Result<()> {
    let project_path = session
        .project_path
        .as_deref()
        .map(|path| path.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
        .transpose()?;
    let event_count =
        i64::try_from(session.event_count).map_err(|_| StoreError::InvalidSessionReference)?;
    statement
        .execute(params![
            provider,
            session.session.id,
            session.title.as_deref(),
            project_path,
            session.git_branch.as_deref(),
            session.created_at.as_ref().map(DateTime::timestamp_millis),
            session.updated_at.as_ref().map(DateTime::timestamp_millis),
            session.updated_at_approximate,
            event_count,
            indexed_at,
            session
                .source_revision
                .and_then(|r| r.file)
                .map(Uuid::into_bytes),
            session
                .source_revision
                .and_then(|r| r.contents)
                .map(Uuid::into_bytes),
        ])
        .map_err(database_error)?;
    Ok(())
}

fn cached_content_hash(connection: &Connection, session: &SessionRef) -> Result<Option<Vec<u8>>> {
    connection
        .query_row(
            "SELECT content_hash FROM session_trajectories WHERE provider=?1 AND session_id=?2",
            params![session.provider.to_string(), session.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(database_error)
}

fn cached_trajectory_state(
    connection: &Connection,
    session: &SessionRef,
) -> Result<Option<TrajectoryIndexState>> {
    validate_session_ref(session)?;
    let row = connection
        .query_row(
            "SELECT source_updated_at, source_complete, document_version, source_updated_submillis, provider_limited, native_summary, source_file_id, missing_ancestor_id, source_contents_id FROM session_trajectories
             WHERE provider = ?1 AND session_id = ?2",
            params![session.provider.to_string(), session.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, source_revision_from_row(row, 6)?)),
        )
        .optional()
        .map_err(database_error)?;
    row.map(
        |(
            updated_at,
            source_complete,
            document_version,
            remainder,
            provider_limited,
            native_summary,
            source_revision,
        )| {
            Ok(TrajectoryIndexState {
                source_updated_at: source_time_from_db(updated_at, remainder)?,
                source_time_precise: remainder.is_some(),
                source_complete,
                provider_limited,
                native_summary,
                source_revision,
                document_version,
            })
        },
    )
    .transpose()
}

fn cached_session(
    connection: &Connection,
    session: &SessionRef,
    observed: bool,
) -> Result<Option<IndexedSession>> {
    connection
        .query_row(
            "SELECT provider, session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count, source_file_id, source_contents_id
             FROM raw_session_catalog WHERE provider = ?1 AND session_id = ?2 AND CASE WHEN ?3 THEN name_observable ELSE home_matches END",
            params![session.provider.to_string(), session.id, observed],
            indexed_session_from_row,
        )
        .optional()
        .map_err(database_error)
}

fn indexed_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSession> {
    let provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let created_at = optional_timestamp_from_row(row, 5)?;
    let updated_at = optional_timestamp_from_row(row, 6)?;
    let updated_at_approximate = row.get(7)?;
    let event_count = row.get::<_, i64>(8)?;
    let event_count = usize::try_from(event_count).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(error))
    })?;
    let file = row.get::<_, Option<[u8; 16]>>(9)?.map(Uuid::from_bytes);
    let contents = row.get::<_, Option<[u8; 16]>>(10)?.map(Uuid::from_bytes);
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

fn validate_trajectory_document(
    session: &SessionRef,
    document: &TrajectoryDocument<'_>,
    guard: TrajectorySourceGuard,
) -> Result<()> {
    validate_session_ref(session)?;
    if (document.origin == SessionTrajectoryOrigin::NativeSummary
        && (session.provider != Provider::Antigravity
            || !matches!(guard, TrajectorySourceGuard::ObservedDocument(_))
            || !document.source_complete
            || !document.redacted_text.is_empty()
            || document.source_byte_count != 0
            || document.truncation_strategy != "none"))
        || (document.source_complete && document.provider_limited)
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
    Ok(())
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
    connection.create_scalar_function("omnis_compact_search_title", 1, flags, |context| {
        Ok(context
            .get_raw(0)
            .as_str()
            .ok()
            .and_then(omnis_core::compact_search_title))
    })?;
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
    if provider == Provider::Imported {
        0
    } else {
        time.timestamp_subsec_nanos() % 1_000_000
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
        collections::{HashMap, HashSet},
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
        BranchHeadRestore, IndexedSession, LocalOpenerEvidence, SessionDiscoveryClaim,
        SessionDiscoveryState, SessionTrajectoryOrigin, Store, TRAJECTORY_CHUNK_BYTE_LIMIT,
        TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES, TRAJECTORY_PHRASE_MAX_TOKENS,
        TRAJECTORY_QUERY_MAX_TOKEN_BYTES, TRAJECTORY_SEARCH_RESULT_LIMIT, TrajectoryDocument,
        state_root, trajectory_clauses, utf8_chunks,
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
    fn workspace_discovery_keeps_the_provider_check_time_and_other_sessions() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let provider = Provider::Antigravity;
        let first = indexed_session(provider, "first", "First workspace");
        let other = indexed_session(provider, "other", "Other workspace");
        let provider_times = || {
            (
                store.session_index_checked_at(provider).unwrap(),
                store.session_index_refreshed_at(provider).unwrap(),
            )
        };
        store
            .merge_workspace_sessions(provider, std::slice::from_ref(&first))
            .unwrap();
        assert_eq!(provider_times(), (None, None));

        store
            .replace_indexed_sessions(provider, &[first.clone(), other.clone()])
            .unwrap();
        store
            .connection
            .borrow()
            .execute_batch(
                "UPDATE session_index_checks SET checked_at = 1;
             UPDATE session_index_state SET indexed_at = 1;",
            )
            .unwrap();
        let before = provider_times();
        let renamed = IndexedSession {
            title: Some("Renamed".into()),
            ..first
        };
        store
            .merge_workspace_sessions(provider, std::slice::from_ref(&renamed))
            .unwrap();
        store.merge_workspace_sessions(provider, &[]).unwrap();
        let cached = store.indexed_sessions_for_provider(provider).unwrap();
        assert!(cached.contains(&renamed) && cached.contains(&other));
        assert_eq!(provider_times(), before);
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
    fn same_timestamp_preview_does_not_downgrade_full_source_head_tail() {
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
            .expect("same-state preview");

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
                derived_title,
                truncation_strategy: "none",
                ..rollout_document(
                    text,
                    source_updated_at,
                    SourceRevision::default(),
                    true,
                    document_version,
                )
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
            store.trajectory_titles().expect("titles")[&session],
            "Synthetic title"
        );
        let mut incomplete = document("partial projection", 3, None, later);
        incomplete.source_complete = false;
        store
            .upsert_trajectory_document(&session, &incomplete)
            .expect("rebuild preview");
        assert_eq!(
            store.trajectory_titles().expect("preview title")[&session],
            "Synthetic title"
        );
        let mut updated = document("updated projection", 3, None, later);
        updated.title_authoritative = true;
        let guard =
            super::TrajectorySourceGuard::Observed(store.trajectory_index_state(&session).unwrap());
        store
            .upsert_observed_trajectory_documents(&[(&session, updated, guard)])
            .expect("rebuild without a title");
        assert!(
            !store
                .trajectory_titles()
                .expect("titles")
                .contains_key(&session)
        );
    }

    #[test]
    fn a_provider_limit_can_replace_an_older_full_session_during_upgrade() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Hermes, "growing-session");
        let initial = Utc::now();
        let write = |text: &str, seconds, version, full, limited| {
            store
                .upsert_trajectory_document(
                    &session,
                    &TrajectoryDocument {
                        title_authoritative: false,
                        title_is_verified: false,
                        local_opener: LocalOpenerEvidence::Unknown,
                        database_step_proof: None,
                        desktop_projection: None,
                        legacy_utf8_text: None,
                        withdrawn_title: None,
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
    fn a_native_catalog_summary_preserves_its_source_kind_across_same_time_reads() {
        use sha2::{Digest, Sha256};

        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Antigravity, "summary");
        let native = TrajectoryDocument {
            title_authoritative: false,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: "",
            source_updated_at: Utc::now(),
            source_byte_count: 0,
            indexed_byte_count: 0,
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 6,
            derived_title: None,
        };
        let summary = TrajectoryDocument {
            origin: SessionTrajectoryOrigin::NativeSummary,
            ..native
        };
        let write = |doc, guard| {
            store.upsert_observed_trajectory_documents_counted(&[(&session, doc, guard)])
        };
        let observe = || store.observe_trajectory_document(&session).unwrap();
        // Make the receipt comparison independent of the time taken by the test.
        let fix_indexed_at = || {
            store
                .connection
                .borrow()
                .execute("UPDATE session_trajectories SET indexed_at = 1", [])
                .unwrap()
        };
        assert_eq!(write(native, observe()).unwrap(), 1);
        fix_indexed_at();
        let before = observe();
        assert!(matches!(
            store.upsert_trajectory_document(&session, &summary),
            Err(StoreError::InvalidSessionReference)
        ));
        assert_eq!(write(summary, before).unwrap(), 1);
        fix_indexed_at();
        let after = observe();
        assert_ne!(before, after);
        assert!(matches!(
            write(native, before),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(write(summary, after).unwrap(), 0);
        let later = TrajectoryDocument {
            source_updated_at: native.source_updated_at + chrono::Duration::seconds(1),
            ..native
        };
        assert_eq!(
            write(later, super::TrajectorySourceGuard::NewerSource).unwrap(),
            0
        );
        assert_eq!(observe(), after);
        assert!(store.trajectory_index_states().unwrap()[&session].native_summary);
        let (origin, hash): (String, Vec<u8>) = store
            .connection
            .borrow()
            .query_row(
                "SELECT origin, content_hash FROM session_trajectories",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "native");
        assert_eq!(hash, Sha256::digest(b"").to_vec());

        let preview = TrajectoryDocument {
            redacted_text: "history",
            source_byte_count: 7,
            indexed_byte_count: 7,
            source_complete: false,
            ..native
        };
        assert_eq!(write(preview, after).unwrap(), 1);
        let state = store.trajectory_index_state(&session).unwrap().unwrap();
        assert!(!state.native_summary);
        let history = || store.search_session_trajectories("history", 1).unwrap();
        assert_eq!(history(), std::slice::from_ref(&session));
        assert_eq!(write(summary, observe()).unwrap(), 1);
        assert!(history().is_empty());
        assert!(matches!(
            write(
                TrajectoryDocument {
                    origin: SessionTrajectoryOrigin::NativeSummary,
                    ..preview
                },
                observe()
            ),
            Err(StoreError::InvalidSessionReference)
        ));
    }

    #[test]
    fn an_observed_antigravity_read_retires_only_eligible_index_failures() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Antigravity, "summary");
        let summary = TrajectoryDocument {
            title_authoritative: false,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: "",
            source_updated_at: Utc::now(),
            source_byte_count: 0,
            indexed_byte_count: 0,
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::NativeSummary,
            document_version: 6,
            derived_title: None,
        };
        let observe = || store.observe_trajectory_document(&session).unwrap();
        let write = |guard| {
            store.upsert_observed_trajectory_documents_counted(&[(&session, summary, guard)])
        };
        let before = observe();
        assert_eq!(write(before).unwrap(), 1);
        for (native_summary, document_version) in [(false, 6), (true, 5), (false, 7)] {
            let failure = super::TrajectoryIndexFailure {
                source_file: None,
                source_contents: None,
                source_updated_at: Some(summary.source_updated_at),
                source_time_precise: true,
                native_summary,
                document_version,
            };
            store
                .record_trajectory_index_failure(&session, &failure)
                .unwrap();
            assert!(matches!(
                write(before),
                Err(StoreError::TrajectorySourceChanged)
            ));
            assert_eq!(
                store.trajectory_index_failures().unwrap()[&session],
                failure
            );
            let current = observe();
            assert_eq!(write(current).unwrap(), 0);
            assert_eq!(
                store.trajectory_index_failures().unwrap().get(&session),
                (document_version > summary.document_version).then_some(&failure)
            );
            assert_eq!(write(observe()).unwrap(), 0);
        }
    }

    #[test]
    fn native_summary_provenance_defaults_to_false_for_legacy_documents_and_failures() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let session = SessionRef::new(Provider::Antigravity, "summary");
        let time = Utc::now();
        let failure = super::TrajectoryIndexFailure {
            source_file: None,
            source_contents: None,
            source_updated_at: Some(time),
            source_time_precise: true,
            native_summary: false,
            document_version: 6,
        };
        let store = Store::open(&path).unwrap();
        store
            .upsert_session_trajectory(&session, "original", time, true)
            .unwrap();
        store
            .record_trajectory_index_failure(&session, &failure)
            .unwrap();
        store
            .connection
            .borrow()
            .execute_batch(
                "
            ALTER TABLE session_trajectories DROP COLUMN native_summary;
            ALTER TABLE trajectory_index_failures DROP COLUMN native_summary;
        ",
            )
            .unwrap();
        drop(store);

        let store = Store::open(path).unwrap();
        let state = store.trajectory_index_state(&session).unwrap().unwrap();
        let recorded = || store.trajectory_index_failures().unwrap()[&session];
        assert!(!state.native_summary);
        assert_eq!(recorded(), failure);
        for native_summary in [true, false] {
            let changed = super::TrajectoryIndexFailure {
                native_summary,
                ..failure
            };
            store
                .record_trajectory_index_failure(&session, &changed)
                .unwrap();
            assert_eq!(recorded(), changed);
        }
    }

    #[test]
    fn database_restores_require_the_document_observed_before_reading() {
        let temporary = tempdir().unwrap();
        let database = temporary.path().join("store.sqlite3");
        let first = Store::open(&database).unwrap();
        let second = Store::open(&database).unwrap();
        let session = SessionRef::new(Provider::Hermes, "restored");
        let sibling = SessionRef::new(Provider::Hermes, "unrelated");
        let now = Utc::now();
        let make = |text: &'static str, seconds, version, full| TrajectoryDocument {
            title_authoritative: false,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at: now + chrono::Duration::seconds(seconds),
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: full,
            provider_limited: !full,
            origin: SessionTrajectoryOrigin::Native,
            document_version: version,
            derived_title: None,
        };
        first
            .upsert_trajectory_document(&session, &make("before", 0, 4, true))
            .unwrap();
        let original = first.observe_trajectory_document(&session).unwrap();
        let restored = make("restored", -60, 6, false);
        assert_eq!(
            second
                .upsert_observed_trajectory_documents_counted(&[(&session, restored, original)])
                .unwrap(),
            1
        );
        let current = first.observe_trajectory_document(&session).unwrap();
        assert_eq!(
            second
                .upsert_observed_trajectory_documents_counted(&[(&session, restored, current)])
                .unwrap(),
            0
        );
        assert_eq!(
            first.observe_trajectory_document(&session).unwrap(),
            current
        );
        let delayed = make("superseded", -60, 6, true);
        assert!(matches!(
            first.upsert_observed_trajectory_documents_counted(&[
                (
                    &sibling,
                    delayed,
                    first.observe_trajectory_document(&sibling).unwrap()
                ),
                (&session, delayed, original),
            ]),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(first.trajectory_index_state(&sibling).unwrap().is_none());
        assert_eq!(
            first.search_session_trajectories("restored", 1).unwrap(),
            [session]
        );
    }

    #[test]
    fn a_verified_full_read_can_replace_text_that_now_needs_truncation() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Hermes, "same-clock-edit");
        let original = TrajectoryDocument {
            title_authoritative: false,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: "removed",
            source_updated_at: Utc::now(),
            source_byte_count: 7,
            indexed_byte_count: 7,
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 6,
            derived_title: None,
        };
        store
            .upsert_trajectory_document(&session, &original)
            .unwrap();
        let replacement = TrajectoryDocument {
            redacted_text: "newtail",
            source_byte_count: 100,
            truncation_strategy: "event_head_tail",
            ..original
        };
        let observed = store.observe_trajectory_document(&session).unwrap();
        let write = |document, guard| {
            store
                .upsert_observed_trajectory_documents_counted(&[(&session, document, guard)])
                .unwrap()
        };
        assert_eq!(
            write(
                TrajectoryDocument {
                    source_complete: false,
                    ..replacement
                },
                observed
            ),
            0
        );
        assert_eq!(
            write(
                replacement,
                super::TrajectorySourceGuard::Observed(
                    store.trajectory_index_state(&session).unwrap()
                )
            ),
            0
        );
        assert_eq!(write(replacement, observed), 1);
        assert!(
            store
                .search_session_trajectories("removed", 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search_session_trajectories("newtail", 1).unwrap(),
            std::slice::from_ref(&session)
        );
        assert_eq!(
            write(
                replacement,
                store.observe_trajectory_document(&session).unwrap()
            ),
            0
        );
    }

    #[test]
    fn upgrade_reindexes_unproven_titles_and_keeps_current_failures() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        store.connection.borrow().execute_batch(
            "DELETE FROM session_trajectory_migrations WHERE name = 'reverify_legacy_codex_titles';
             INSERT INTO session_index(provider, session_id, title, event_count, indexed_at, source_file_id)
             VALUES ('codex', 'named', '  named  ', 0, 1, NULL),
                    ('codex', 'different', 'different', 0, 1, zeroblob(16));
             WITH legacy(provider, id, full, strategy, origin, title) AS (VALUES
                ('codex', 'sampled', 0, 'none', 'native', 'sampled'),
                ('codex', 'cleared', 1, 'source_incomplete', 'native', NULL),
                ('codex', 'complete', 1, 'none', 'native', 'complete'),
                ('codex', 'header-only', 1, 'none', 'native', 'Ghost opener'),
                ('codex', 'empty', 1, 'none', 'native', NULL),
                ('codex', 'named', 0, 'none', 'native', 'named'),
                ('codex', 'different', 0, 'none', 'native', 'different'),
                ('codex', 'imported', 0, 'none', 'imported_bundle', 'imported'),
                ('claude', 'other', 0, 'none', 'native', 'other')
             )
             INSERT INTO session_trajectories
                (provider, session_id, redacted_text, content_hash, source_updated_at,
                 source_complete, complete, indexed_at, source_byte_count, indexed_byte_count,
                 truncation_strategy, origin, protected_by_bundle, document_version, derived_title)
             SELECT provider, id, '', X'00', 1, full, full AND strategy = 'none', 1, 0, 0,
                    strategy, origin, 0, 5, title FROM legacy;
             UPDATE session_trajectories SET source_file_id = zeroblob(16), source_contents_id = zeroblob(16)
             WHERE session_id IN ('sampled', 'header-only');
             ALTER TABLE session_trajectories DROP COLUMN derived_title_is_verified;
             UPDATE session_trajectories SET source_byte_count=8, indexed_byte_count=8 WHERE session_id='complete';
             INSERT INTO session_trajectory_chunk_rows(trajectory_id, chunk_index, stored_text)
             SELECT id, 0, 'complete' FROM session_trajectories WHERE session_id='complete';
             INSERT INTO trajectory_index_failures(provider, session_id, source_updated_at, document_version, failed_at)
             VALUES ('codex', 'sampled', 1, 5, 1);",
        ).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let titles = store.trajectory_titles().unwrap();
        assert_eq!(
            titles.into_values().collect::<HashSet<_>>(),
            HashSet::from(["named", "imported", "other"].map(str::to_owned))
        );
        let named_verified: bool = store.connection.borrow().query_row(
            "SELECT derived_title_is_verified FROM session_trajectories WHERE provider='codex' AND session_id='named'",
            [], |row| row.get(0),
        ).unwrap();
        assert!(named_verified);
        // The cached text does not identify who said it.
        let complete = SessionRef::new(Provider::Codex, "complete");
        assert_eq!(
            store.search_session_trajectories("complete", 10).unwrap(),
            vec![complete]
        );
        let states = store.trajectory_index_states().unwrap();
        let stale = states
            .into_iter()
            .filter_map(|(session, state)| (state.document_version == 0).then_some(session.id))
            .collect::<HashSet<_>>();
        assert_eq!(
            stale,
            HashSet::from(
                ["sampled", "cleared", "complete", "different", "header-only"].map(str::to_owned)
            )
        );
        let failed = SessionRef::new(Provider::Codex, "sampled");
        assert_eq!(
            store
                .trajectory_index_failure(&failed)
                .unwrap()
                .unwrap()
                .document_version,
            5
        );
    }

    #[test]
    fn upgrade_rechecks_complete_codex_coverage_once_after_the_title_repair() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let native = SessionRef::new(Provider::Codex, "assistant-first");
        let bundle = SessionRef::new(Provider::Codex, "imported");
        let other = SessionRef::new(Provider::Claude, "unaffected");
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..Default::default()
        };
        let complete = TrajectoryDocument {
            truncation_strategy: "none",
            ..rollout_document("Searchable answer", Utc::now(), revision, true, 5)
        };
        for (session, origin) in [
            (&native, SessionTrajectoryOrigin::Native),
            (&bundle, SessionTrajectoryOrigin::ImportedBundle),
            (&other, SessionTrajectoryOrigin::Native),
        ] {
            store
                .upsert_trajectory_document(session, &TrajectoryDocument { origin, ..complete })
                .unwrap();
        }
        store.connection.borrow().execute(
            "DELETE FROM session_trajectory_migrations WHERE name = 'recheck_v5_codex_coverage'",
            [],
        ).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let native_state = |store: &Store| store.trajectory_index_state(&native).unwrap().unwrap();
        let state = native_state(&store);
        let matches = |store: &Store| {
            store
                .search_session_trajectory_matches("answer", 10)
                .unwrap()
        };
        assert_eq!(state.document_version, 0);
        assert_eq!(state.source_revision, revision);
        assert!(state.source_complete);
        let hits = matches(&store);
        assert_eq!(hits.len(), 3);
        for hit in hits {
            let current = hit.session != native;
            assert_eq!(hit.complete, current);
            assert_eq!(
                hit.truncation_strategy,
                if current { "none" } else { "source_incomplete" }
            );
        }

        store
            .upsert_trajectory_document(&native, &complete)
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(native_state(&store).document_version, 5);
        assert!(matches(&store).iter().all(|hit| hit.complete));
    }

    #[test]
    fn title_repair_preserves_the_version_of_a_full_unstamped_v4_projection() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let cases = [
            ("release", 4, true, 4),
            ("older", 0, true, 0),
            ("different-projection", 5, true, 0),
            ("sampled", 4, false, 0),
        ];
        for (id, version, full, _) in cases {
            let document = TrajectoryDocument {
                derived_title: Some("Unverified opener"),
                ..rollout_document(
                    "Middle marker",
                    Utc::now(),
                    SourceRevision::default(),
                    full,
                    version,
                )
            };
            store
                .upsert_trajectory_document(&SessionRef::new(Provider::Codex, id), &document)
                .unwrap();
        }
        store.connection.borrow().execute_batch(
            "DELETE FROM session_trajectory_migrations WHERE name = 'reverify_legacy_codex_titles';
             ALTER TABLE session_trajectories DROP COLUMN derived_title_is_verified;",
        ).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        for (id, _, _, expected) in cases {
            let session = SessionRef::new(Provider::Codex, id);
            assert!(store.trajectory_title(&session).unwrap().is_none(), "{id}");
            assert_eq!(
                store
                    .trajectory_index_state(&session)
                    .unwrap()
                    .unwrap()
                    .document_version,
                expected,
                "{id}"
            );
        }
    }

    #[test]
    fn observed_reads_keep_known_titles_until_the_full_source_can_clear_them() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Claude, "sampled-title");
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
        ] {
            let time = first + chrono::Duration::seconds(offset);
            let text = "Synthetic visible text";
            let document = TrajectoryDocument {
                title_is_verified: false,
                local_opener: LocalOpenerEvidence::Unknown,
                database_step_proof: None,
                desktop_projection: None,
                legacy_utf8_text: None,
                withdrawn_title: None,
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
                title_authoritative: title.is_some()
                    || (full_read && strategy != "source_incomplete"),
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
            prior = store.trajectory_index_state(&session).unwrap();
        }
    }

    #[test]
    fn observed_writes_change_titles_only_when_the_first_title_is_known() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("store.sqlite3");
        let session = SessionRef::new(Provider::Codex, "sampled-title");
        let first = Utc::now();
        let mut prior = None;
        let original = Some("Original request");
        let updated = Some("Updated request");
        let reads = [
            (0, false, "none", false, updated),
            (0, true, "none", true, original),
            (1, false, "none", false, None),
            (2, false, "none", false, updated),
            // Both reads omit events. The second read omitted only tools.
            (3, true, "source_incomplete", false, updated),
            (3, true, "source_incomplete", true, None),
            (4, false, "none", true, updated),
            (4, true, "document_head_tail", true, None),
        ];
        let titles = reads.map(
            |(offset, full_read, strategy, title_authoritative, title)| {
                let store = Store::open(&path).unwrap();
                let time = first + chrono::Duration::seconds(offset);
                let text = "Synthetic visible text";
                let document = TrajectoryDocument {
                    provider_limited: false,
                    redacted_text: text,
                    source_updated_at: time,
                    source_revision: SourceRevision::default(),
                    source_byte_count: text.len() + usize::from(strategy == "document_head_tail"),
                    indexed_byte_count: text.len(),
                    source_complete: full_read,
                    truncation_strategy: strategy,
                    origin: SessionTrajectoryOrigin::Native,
                    document_version: 4,
                    derived_title: title,
                    title_authoritative,
                    title_is_verified: false,
                    local_opener: LocalOpenerEvidence::Unknown,
                    database_step_proof: None,
                    desktop_projection: None,
                    legacy_utf8_text: None,
                    withdrawn_title: None,
                };
                store
                    .upsert_observed_trajectory_documents(&[(
                        &session,
                        document,
                        super::TrajectorySourceGuard::Observed(prior),
                    )])
                    .unwrap();
                prior = store.trajectory_index_state(&session).unwrap();
                store.trajectory_title(&session).unwrap()
            },
        );
        assert_eq!(
            titles.each_ref().map(Option::as_deref),
            [
                None, original, original, original, original, None, updated, None
            ]
        );
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

    fn rollout_document(
        text: &str,
        time: DateTime<Utc>,
        revision: SourceRevision,
        full: bool,
        version: u32,
    ) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            provider_limited: false,
            redacted_text: text,
            source_updated_at: time,
            source_revision: revision,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "source_incomplete",
            source_complete: full,
            origin: SessionTrajectoryOrigin::Native,
            document_version: version,
            derived_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            title_authoritative: false,
        }
    }

    fn seed_uncataloged_title(
        store: &Store,
    ) -> (IndexedSession, TrajectoryDocument<'static>, SessionRef) {
        let time = DateTime::from_timestamp_millis(10).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let row = IndexedSession {
            title: None,
            source_revision: Some(revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "uncataloged", "")
        };
        let doc = TrajectoryDocument {
            derived_title: Some("Unknown title source"),
            title_authoritative: true,
            truncation_strategy: "none",
            ..rollout_document("surviving middle text", time, revision, true, 6)
        };
        store
            .upsert_trajectory_document(&row.session, &doc)
            .unwrap();
        let removed = SessionRef::new(Provider::Codex, "gone");
        store.upsert_trajectory_document(&removed, &doc).unwrap();
        store
            .upsert_indexed_session(&IndexedSession {
                session: removed.clone(),
                ..row.clone()
            })
            .unwrap();
        // The earlier source had a verified title but no field for its origin.
        store.connection.borrow().execute(
            "UPDATE session_trajectories SET derived_title_is_local_opener = NULL WHERE session_id = ?1",
            [&row.session.id],
        ).unwrap();
        (row, doc, removed)
    }

    #[test]
    fn a_first_unnamed_catalog_row_rechecks_only_its_source_after_names_are_complete() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let (row, doc, removed) = seed_uncataloged_title(&store);
        let rows = std::slice::from_ref(&row);
        let before = store.trajectory_index_state(&row.session).unwrap().unwrap();
        let observed = store.observe_codex_catalog_inventory().unwrap();
        store
            .refresh_codex_catalog(rows, true, false, &observed.observed)
            .unwrap();
        let candidate = TrajectoryDocument {
            derived_title: Some("Visible local opener"),
            local_opener: LocalOpenerEvidence::Current,
            title_authoritative: false,
            ..doc
        };
        assert_eq!(
            save_observed_title(&store, &row.session, candidate).as_deref(),
            doc.derived_title
        );
        assert!(store.trajectory_index_state(&removed).unwrap().is_none());
        assert_eq!(
            store.indexed_session(&row.session).unwrap().as_ref(),
            Some(&row)
        );
        assert_eq!(
            store.trajectory_index_state(&row.session).unwrap(),
            Some(before)
        );
        let prior_check = store.session_index_checked_at(Provider::Codex).unwrap();
        let refreshed = store.session_index_refreshed_at(Provider::Codex).unwrap();
        assert!(refreshed.is_some());
        store.merge_codex_search_catalog(rows, true, rows).unwrap();
        assert_eq!(
            store.indexed_session(&row.session).unwrap().as_ref(),
            Some(&row)
        );
        assert_eq!(store.trajectory_title(&row.session).unwrap(), None);
        assert_eq!(
            store.trajectory_index_state(&row.session).unwrap(),
            Some(super::TrajectoryIndexState {
                document_version: 0,
                ..before
            })
        );
        assert_eq!(
            store
                .search_session_trajectories("surviving middle text", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.session_index_checked_at(Provider::Codex).unwrap(),
            prior_check
        );
        assert_eq!(
            store.session_index_refreshed_at(Provider::Codex).unwrap(),
            refreshed
        );

        let checked = TrajectoryDocument {
            derived_title: Some("Known opener"),
            ..doc
        };
        store
            .upsert_trajectory_document(&row.session, &checked)
            .unwrap();
        store.merge_codex_search_catalog(rows, true, rows).unwrap();
        assert_eq!(
            store.trajectory_index_state(&row.session).unwrap(),
            Some(before)
        );
        assert_eq!(
            store.trajectory_title(&row.session).unwrap().as_deref(),
            Some("Known opener")
        );
    }

    #[test]
    fn discovery_binds_only_the_observed_legacy_document_to_its_home() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("store.sqlite3");
        let a = Store::open(&database).unwrap();
        let b = Store::open(&database).unwrap();
        b.connection
            .borrow()
            .execute("UPDATE current_codex_home SET home='/other'", [])
            .unwrap();
        let seed = |id, file| {
            let (mut row, doc) =
                old_explicit_title(&a, id, Uuid::from_u128(file), Some("Old label"));
            row.updated_at =
                DateTime::from_timestamp_millis(doc.source_updated_at.timestamp_millis());
            a.upsert_indexed_session(&row).unwrap();
            a.connection
                .borrow()
                .execute(
                    "UPDATE session_index SET codex_home=NULL WHERE session_id=?1",
                    [id],
                )
                .unwrap();
            (row, doc)
        };
        let (row, _) = seed("matched", 1);
        let (changed, doc) = seed("changed", 2);
        let (opened, opened_doc) = seed("opened", 4);
        let observation = a.observe_codex_catalog_inventory().unwrap();
        let (late, _) = seed("late", 3);
        a.upsert_trajectory_document(
            &changed.session,
            &TrajectoryDocument {
                source_revision: SourceRevision {
                    contents: Some(Uuid::from_u128(200)),
                    ..doc.source_revision
                },
                source_updated_at: doc.source_updated_at + chrono::Duration::seconds(1),
                ..doc
            },
        )
        .unwrap();
        let partial_name = IndexedSession {
            title: None,
            ..row.clone()
        };
        a.merge_codex_search_catalog_with_name_history(
            &[partial_name, changed.clone(), late.clone()],
            false,
            &observation.observed,
            &HashMap::from([(row.session.id.clone(), "Old label".to_owned())]),
        )
        .unwrap();
        assert!(b.indexed_session(&row.session).unwrap().is_none());
        for session in [&changed.session, &late.session] {
            assert!(b.indexed_session(session).unwrap().is_none());
        }
        let (cached, titles) = b.cached_picker_sessions_with_titles().unwrap();
        assert!(cached.is_empty() && titles.is_empty());

        let before_open = a.observe_selected_session(&opened.session).unwrap();
        a.upsert_selected_session_document(&opened, &before_open, opened_doc)
            .unwrap();
        assert!(b.indexed_session(&opened.session).unwrap().is_none());

        let foreign = b.observe_codex_catalog_inventory().unwrap();
        b.refresh_codex_catalog(&[], true, true, &foreign.observed)
            .unwrap();
        assert_eq!(a.indexed_session(&row.session).unwrap(), Some(row.clone()));
        assert_eq!(
            a.indexed_session(&opened.session).unwrap(),
            Some(opened.clone())
        );
        assert!(a.trajectory_index_state(&row.session).unwrap().is_some());
        a.connection
            .borrow()
            .execute_batch(
                "CREATE TEMP TRIGGER no_warm_catalog_updates BEFORE UPDATE ON session_index
            BEGIN SELECT RAISE(ABORT,'unchanged catalog was rewritten'); END",
            )
            .unwrap();
        let now = a.observe_codex_catalog_inventory().unwrap();
        a.refresh_codex_catalog(&[row, opened.clone()], true, true, &now.observed)
            .unwrap();
        let warm = a.observe_selected_session(&opened.session).unwrap();
        a.upsert_selected_session_document(&opened, &warm, opened_doc)
            .unwrap();
    }

    #[test]
    fn background_index_binds_a_legacy_catalog_only_when_it_writes_the_selected_source() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp_millis(100).unwrap();
        let old = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let new = SourceRevision {
            contents: Some(Uuid::from_u128(3)),
            ..old
        };
        let catalog = IndexedSession {
            title: None,
            source_revision: Some(new),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "appended", "")
        };
        let session = &catalog.session;
        store
            .upsert_trajectory_document(
                session,
                &rollout_document("older body", time, old, true, 6),
            )
            .unwrap();
        store.upsert_indexed_session(&catalog).unwrap();
        let remove_home = || {
            store
                .connection
                .borrow()
                .execute("UPDATE session_index SET codex_home=NULL", [])
                .unwrap();
        };
        remove_home();
        let observed = store.observe_codex_catalog_inventory().unwrap();
        store
            .refresh_codex_catalog_with_name_history(
                std::slice::from_ref(&catalog),
                true,
                true,
                &observed,
                &HashMap::new(),
            )
            .unwrap();
        assert!(store.indexed_session(session).unwrap().is_none());

        let observed_source = || {
            super::TrajectorySourceGuard::Observed(store.trajectory_index_state(session).unwrap())
        };
        let publish = |complete, guard| {
            let doc = rollout_document("appended rookery", time, new, complete, 6);
            store
                .upsert_discovered_trajectory_documents(&[(session, doc, guard, None)], &[&catalog])
        };
        assert!(matches!(
            publish(false, super::TrajectorySourceGuard::Observed(None)),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(store.indexed_session(session).unwrap().is_none());
        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_chunk BEFORE INSERT ON session_trajectory_chunk_rows
             BEGIN SELECT raise(ABORT,'cannot publish the document'); END;",
            )
            .unwrap();
        assert!(publish(false, observed_source()).is_err());
        assert!(store.indexed_session(session).unwrap().is_none());
        store
            .connection
            .borrow()
            .execute_batch("DROP TRIGGER fail_chunk")
            .unwrap();
        publish(false, observed_source()).unwrap();
        assert_eq!(
            store.indexed_session(session).unwrap().as_ref(),
            Some(&catalog)
        );
        assert_eq!(
            store.search_session_trajectories("rookery", 10).unwrap(),
            vec![session.clone()]
        );
        publish(true, observed_source()).unwrap();
        remove_home();
        let full = store.trajectory_index_state(session).unwrap();
        publish(false, observed_source()).unwrap();
        assert_eq!(store.trajectory_index_state(session).unwrap(), full);
        assert!(store.indexed_session(session).unwrap().is_none());
    }

    #[test]
    fn a_catalog_point_lookup_skips_unrelated_sessions() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let chosen = indexed_session(Provider::Codex, "chosen", "Chosen title");
        store.upsert_indexed_session(&chosen).unwrap();
        let connection = store.connection.borrow();
        let mut statement = connection.prepare(super::SELECTED_CATALOG_SQL).unwrap();
        let mut lookup = || {
            let title: String = statement
                .query_row(["codex", "chosen"], |row| row.get(2))
                .unwrap();
            assert_eq!(title, "Chosen title");
            statement.reset_status(rusqlite::StatementStatus::VmStep)
        };
        let before = lookup();
        connection.execute_batch(
            "WITH RECURSIVE sessions(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM sessions WHERE id<256)
             INSERT INTO session_index(provider,session_id,codex_home,event_count,indexed_at)
             SELECT 'codex','unrelated-' || id,(SELECT home FROM current_codex_home),0,0 FROM sessions;",
        ).unwrap();
        let after = lookup();
        assert!(
            after < before * 2,
            "point lookup took {before} steps before adding unrelated sessions and {after} afterward"
        );
    }

    #[test]
    fn a_shared_codex_file_does_not_carry_a_name_between_homes() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        for home in [None, Some("/foreign")] {
            for direct in [false, true] {
                let (mut row, doc) =
                    old_explicit_title(&store, "shared", Uuid::from_u128(7), Some("Old label"));
                row.updated_at =
                    DateTime::from_timestamp_millis(doc.source_updated_at.timestamp_millis());
                store.upsert_indexed_session(&row).unwrap();
                store
                    .connection
                    .borrow()
                    .execute("UPDATE session_index SET codex_home=?1", [home])
                    .unwrap();
                row.title = None;
                if direct {
                    let observed = store.observe_selected_session(&row.session).unwrap();
                    assert!(observed.metadata.is_none());
                    let inconclusive = TrajectoryDocument {
                        derived_title: None,
                        title_is_verified: false,
                        ..doc
                    };
                    store
                        .upsert_selected_session_document_with_name_coverage(
                            &row,
                            &observed,
                            inconclusive,
                            None,
                            false,
                        )
                        .unwrap();
                } else {
                    let inventory = store.observe_codex_catalog_inventory().unwrap();
                    assert!(inventory.observable_names.is_empty());
                    store
                        .refresh_codex_catalog_with_name_history(
                            std::slice::from_ref(&row),
                            true,
                            false,
                            &inventory,
                            &HashMap::new(),
                        )
                        .unwrap();
                }
                assert_eq!(
                    store.indexed_session(&row.session).unwrap(),
                    Some(row.clone())
                );
                assert_eq!(store.trajectory_title(&row.session).unwrap(), None);
                assert!(
                    store
                        .trajectory_index_state(&row.session)
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn complete_discovery_retires_only_the_unstamped_document_it_observed() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let catalog_time = DateTime::from_timestamp_millis(100).unwrap();
        let document_time = DateTime::from_timestamp_millis(200).unwrap();
        let ids = ["missing", "changed", "inserted", "stamped", "protected"];
        for id in ids {
            let row = IndexedSession {
                updated_at: Some(catalog_time),
                ..indexed_session(Provider::Codex, id, id)
            };
            store.upsert_indexed_session(&row).unwrap();
            if id != "inserted" {
                store
                    .upsert_session_trajectory(&row.session, "older lemur", document_time, true)
                    .unwrap();
            }
        }
        let observed = store.observe_codex_catalog_inventory().unwrap();
        let session = |id: &str| SessionRef::new(Provider::Codex, id);
        store
            .upsert_session_trajectory(&session("inserted"), "recent lynx", catalog_time, true)
            .unwrap();
        store
            .upsert_session_trajectory(&session("changed"), "recent lynx", document_time, true)
            .unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..Default::default()
        };
        store
            .upsert_trajectory_document(
                &session("stamped"),
                &rollout_document("recent lynx", document_time, revision, true, 6),
            )
            .unwrap();
        store.connection.borrow().execute("UPDATE session_trajectories SET protected_by_bundle=1 WHERE session_id='protected'",[]).unwrap();

        store
            .refresh_codex_catalog_with_name_history(&[], true, true, &observed, &HashMap::new())
            .unwrap();
        assert!(
            store
                .indexed_sessions_for_provider(Provider::Codex)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search_session_trajectories("older lemur", 10)
                .unwrap(),
            vec![session("protected")]
        );
        let current: HashSet<_> = store
            .search_session_trajectories("recent lynx", 10)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            current,
            HashSet::from([session("changed"), session("inserted"), session("stamped")])
        );
    }

    #[test]
    fn complete_codex_discovery_prunes_only_missing_rows_in_its_home() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp_millis(100).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let catalog = |id| IndexedSession {
            source_revision: Some(revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, id, id)
        };
        let legacy = catalog("legacy");
        let missing = catalog("missing");
        let foreign = catalog("foreign");
        for row in [&legacy, &missing, &foreign] {
            store.upsert_indexed_session(row).unwrap();
        }
        store
            .connection
            .borrow()
            .execute_batch(
                "UPDATE session_index SET codex_home=NULL WHERE session_id='legacy';
             UPDATE session_index SET codex_home='/other' WHERE session_id='foreign';",
            )
            .unwrap();
        let orphan = catalog("orphan");
        for row in [&missing, &foreign, &orphan] {
            let doc = rollout_document(&row.session.id, time, revision, true, 6);
            store
                .upsert_trajectory_document(&row.session, &doc)
                .unwrap();
        }
        let failure = super::TrajectoryIndexFailure {
            source_time_precise: true,
            native_summary: false,
            source_updated_at: Some(time),
            source_file: revision.file,
            source_contents: revision.contents,
            document_version: 6,
        };
        store
            .record_trajectory_index_failure(&legacy.session, &failure)
            .unwrap();
        let inventory = store.observe_codex_catalog_inventory().unwrap();
        assert!(!inventory.observable_names.contains_key(&legacy.session));
        let raw = inventory.observed;
        assert_eq!(raw.len(), 2);
        store.refresh_codex_catalog(&[], false, true, &raw).unwrap();
        store.merge_codex_search_catalog(&[], true, &raw).unwrap();
        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::Codex)
                .unwrap(),
            vec![missing.clone()]
        );

        store.refresh_codex_catalog(&[], true, false, &raw).unwrap();
        assert!(
            store
                .indexed_sessions_for_provider(Provider::Codex)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .trajectory_index_state(&missing.session)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .trajectory_index_state(&orphan.session)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.trajectory_index_failure(&legacy.session).unwrap(),
            Some(failure)
        );
        store
            .connection
            .borrow()
            .execute("UPDATE current_codex_home SET home='/other'", [])
            .unwrap();
        assert_eq!(
            store.indexed_session(&foreign.session).unwrap(),
            Some(foreign.clone())
        );
        assert!(
            store
                .trajectory_index_state(&foreign.session)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn codex_discovery_checks_a_legacy_row_before_removing_or_updating_it() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let legacy = IndexedSession {
            source_revision: Some(SourceRevision {
                file: Some(Uuid::from_u128(1)),
                contents: Some(Uuid::from_u128(2)),
                ..SourceRevision::default()
            }),
            ..indexed_session(Provider::Codex, "legacy", "Original")
        };
        store.upsert_indexed_session(&legacy).unwrap();
        store
            .connection
            .borrow()
            .execute("UPDATE session_index SET codex_home=NULL", [])
            .unwrap();
        let observed = store.observe_codex_catalog_inventory().unwrap().observed;
        store
            .connection
            .borrow()
            .execute("UPDATE session_index SET title='Concurrent'", [])
            .unwrap();
        for (incoming, complete) in [
            (std::slice::from_ref(&legacy), false),
            (std::slice::from_ref(&legacy), true),
            (&[][..], true),
        ] {
            assert!(matches!(
                store.refresh_codex_catalog(incoming, complete, true, &observed),
                Err(super::StoreError::TrajectorySourceChanged)
            ));
        }
        let observed = store.observe_codex_catalog_inventory().unwrap().observed;
        assert_eq!(observed[0].title.as_deref(), Some("Concurrent"));
        store
            .refresh_codex_catalog(&[], true, true, &observed)
            .unwrap();
        assert!(store.indexed_session(&legacy.session).unwrap().is_none());
        assert_eq!(
            store.observe_codex_catalog_inventory().unwrap().observed,
            observed
        );
    }

    #[test]
    fn codex_catalog_rename_rejects_delayed_discovery_and_index_reads() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        store.refresh_codex_catalog(&[], true, true, &[]).unwrap();
        assert!(
            store
                .session_index_refreshed_at(Provider::Codex)
                .unwrap()
                .is_some()
        );

        let time = DateTime::from_timestamp_millis(100).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let old = IndexedSession {
            source_revision: Some(revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "named", "Amber Heron")
        };
        let session = &old.session;
        let first = std::slice::from_ref(&old);
        store.refresh_codex_catalog(first, true, true, &[]).unwrap();
        let doc = TrajectoryDocument {
            title_authoritative: true,
            derived_title: Some("Amber Heron"),
            ..rollout_document("searchable body", time, revision, true, 5)
        };
        store.upsert_trajectory_document(session, &doc).unwrap();
        let observed = store
            .indexed_sessions_for_provider(Provider::Codex)
            .unwrap();
        let prior = store.trajectory_index_state(session).unwrap();
        let renamed = IndexedSession {
            title: Some("Violet   Quokka".into()),
            ..old.clone()
        };
        store
            .refresh_codex_catalog(std::slice::from_ref(&renamed), true, true, &observed)
            .unwrap();

        assert!(matches!(
            store.refresh_codex_catalog(first, true, true, &observed),
            Err(super::StoreError::TrajectorySourceChanged)
        ));
        let delayed = [(
            session,
            doc,
            super::TrajectorySourceGuard::Observed(prior),
            None,
        )];
        assert!(matches!(
            store.upsert_discovered_trajectory_documents(&delayed, &[&old]),
            Err(super::StoreError::TrajectorySourceChanged)
        ));

        let catalog = store
            .indexed_sessions_for_provider(Provider::Codex)
            .unwrap();
        let checked = store.session_index_refreshed_at(Provider::Codex).unwrap();
        store
            .refresh_codex_catalog(&catalog, true, true, &catalog)
            .unwrap();
        assert_eq!(
            store.session_index_refreshed_at(Provider::Codex).unwrap(),
            checked
        );
        assert_eq!(store.trajectory_index_state(session).unwrap(), prior);
        assert_eq!(
            store.trajectory_title(session).unwrap().as_deref(),
            Some("Violet Quokka")
        );
    }

    #[test]
    fn historical_name_recovery_requires_the_same_observed_catalog_row_and_native_spelling() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp_millis(100).unwrap();
        let old = SourceRevision {
            file: Some(Uuid::from_u128(101)),
            contents: Some(Uuid::from_u128(102)),
            ..SourceRevision::default()
        };
        let assignment = "Azure Quokka password=example";
        let stored_name = omnis_core::redact_secrets(assignment);
        assert!(stored_name.contains("[REDACTED: PASSWORD]"));
        let named = IndexedSession {
            updated_at: Some(time),
            source_revision: Some(old),
            ..indexed_session(Provider::Codex, "legacy-name", &stored_name)
        };
        let empty = store.observe_selected_session(&named.session).unwrap();
        store.upsert_indexed_session(&named).unwrap();
        store
            .connection
            .borrow()
            .execute("UPDATE session_index SET codex_legacy_name=1", [])
            .unwrap();

        let appended = SourceRevision {
            contents: Some(Uuid::from_u128(103)),
            ..old
        };
        let selected = IndexedSession {
            title: None,
            updated_at: Some(DateTime::from_timestamp_millis(200).unwrap()),
            source_revision: Some(appended),
            ..named.clone()
        };
        let cached_title = store
            .cached_sessions_with_titles(Some(Provider::Codex))
            .unwrap()
            .0
            .pop()
            .unwrap()
            .title;
        let picker_title = store
            .cached_picker_sessions_with_titles()
            .unwrap()
            .0
            .pop()
            .unwrap()
            .title;
        assert_eq!((cached_title, picker_title), (named.title.clone(), None));
        let inventory = store.observe_codex_catalog_inventory().unwrap();
        assert_eq!(inventory.legacy_codex_source(&named.session), Some(old));
        assert!(inventory.observable_names.is_empty());
        assert_eq!(
            inventory.corroborated_legacy_codex_name(&selected, assignment),
            named.title.as_deref()
        );
        let observed = store.observe_selected_session(&named.session).unwrap();
        assert!(observed.metadata.is_none());
        assert!(
            observed
                .corroborated_legacy_codex_name(&selected, "Different spelling password=example")
                .is_none()
        );
        let replacement = IndexedSession {
            source_revision: Some(SourceRevision {
                file: Some(Uuid::from_u128(104)),
                ..appended
            }),
            ..selected.clone()
        };
        assert!(!observed.legacy_codex_name_candidate(&replacement));
        let doc = TrajectoryDocument {
            derived_title: Some("Actual opening"),
            title_authoritative: true,
            ..rollout_document("new body", selected.updated_at.unwrap(), appended, true, 6)
        };
        let publish = |observation| {
            store.upsert_selected_session_document_with_name_history(
                &selected,
                observation,
                doc,
                None,
                false,
                Some(assignment),
            )
        };
        assert!(matches!(
            publish(&empty),
            Err(super::StoreError::TrajectorySourceChanged)
        ));
        publish(&observed).unwrap();
        assert_eq!(store.trajectory_title(&named.session).unwrap(), named.title);
        assert!(
            !store
                .observe_codex_catalog_inventory()
                .unwrap()
                .has_legacy_codex_names()
        );
    }

    #[test]
    fn a_catalog_only_name_follows_its_rollout_until_the_first_document_is_written() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp_millis(100).unwrap();
        let first_revision = SourceRevision {
            file: Some(Uuid::from_u128(61)),
            contents: Some(Uuid::from_u128(62)),
            ..Default::default()
        };
        let named = IndexedSession {
            source_revision: Some(first_revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "interrupted", "Azure   Quokka")
        };
        let rows = std::slice::from_ref(&named);
        store.refresh_codex_catalog(rows, true, true, &[]).unwrap();
        let revision = SourceRevision {
            contents: Some(Uuid::from_u128(63)),
            ..first_revision
        };
        let appended = IndexedSession {
            source_revision: Some(revision),
            title: None,
            ..named.clone()
        };
        let observation = store.observe_selected_session(&named.session).unwrap();
        let doc = TrajectoryDocument {
            derived_title: Some("Actual opener"),
            title_is_verified: true,
            local_opener: LocalOpenerEvidence::Current,
            ..rollout_document("updated body", time, revision, true, 6)
        };
        store
            .refresh_codex_catalog(std::slice::from_ref(&appended), true, false, rows)
            .unwrap();
        assert_eq!(
            store
                .indexed_session(&named.session)
                .unwrap()
                .unwrap()
                .title,
            named.title
        );
        check_catalog_name_in_other_home(
            &directory.path().join("store.sqlite3"),
            &named,
            &appended,
            &observation,
            doc,
        );
        write_selected_catalog_document(&store, &appended, doc, false);
        assert_eq!(
            store.trajectory_title(&named.session).unwrap().as_deref(),
            Some("Azure Quokka")
        );

        let catalog = store
            .indexed_sessions_for_provider(Provider::Codex)
            .unwrap();
        store
            .refresh_codex_catalog(std::slice::from_ref(&appended), true, true, &catalog)
            .unwrap();
        assert!(matches!(
            store.upsert_selected_session_document_with_name_coverage(
                &appended,
                &observation,
                doc,
                None,
                false
            ),
            Err(super::StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(store.trajectory_title(&named.session).unwrap(), None);
    }

    fn check_catalog_name_in_other_home(
        path: &std::path::Path,
        named: &IndexedSession,
        appended: &IndexedSession,
        observation: &super::SelectedSessionObservation,
        doc: TrajectoryDocument<'_>,
    ) {
        let store_in_other_home = Store::open(path).unwrap();
        store_in_other_home
            .connection
            .borrow()
            .execute(
                "UPDATE current_codex_home SET home = home || '/sessions/other'",
                [],
            )
            .unwrap();
        assert!(
            store_in_other_home
                .observe_codex_catalog_inventory()
                .unwrap()
                .observable_names
                .is_empty()
        );
        assert!(
            store_in_other_home
                .observe_selected_session(&named.session)
                .unwrap()
                .metadata
                .is_none()
        );
        assert!(matches!(
            store_in_other_home.upsert_selected_session_document_with_name_coverage(
                appended,
                observation,
                doc,
                None,
                false
            ),
            Err(super::StoreError::TrajectorySourceChanged)
        ));
        store_in_other_home
            .connection
            .borrow()
            .execute("UPDATE current_codex_home SET home=NULL", [])
            .unwrap();
        assert!(
            store_in_other_home
                .indexed_session(&named.session)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_unresolved_catalog_name_survives_appends_until_its_removal_is_confirmed() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let name = "same spelling";
        let time = DateTime::from_timestamp_millis(100).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(41)),
            contents: Some(Uuid::from_u128(42)),
            ..Default::default()
        };
        let named = IndexedSession {
            source_revision: Some(revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "named-append", "same   spelling")
        };
        let session = &named.session;
        let initial = TrajectoryDocument {
            derived_title: Some(name),
            title_authoritative: true,
            ..rollout_document("original body", time, revision, true, 6)
        };
        save_observed_title(&store, session, initial);
        store
            .refresh_codex_catalog(std::slice::from_ref(&named), true, true, &[])
            .unwrap();
        let appended_revision = SourceRevision {
            contents: Some(Uuid::from_u128(43)),
            ..revision
        };
        let appended = IndexedSession {
            source_revision: Some(appended_revision),
            title: None,
            ..named.clone()
        };
        let observation = store.observe_selected_session(session).unwrap();
        let partial = TrajectoryDocument {
            redacted_text: "appended body",
            source_revision: appended_revision,
            title_authoritative: false,
            title_is_verified: true,
            local_opener: LocalOpenerEvidence::VisiblePrefix,
            derived_title: Some("inherited opener"),
            ..initial
        };
        write_selected_catalog_document(&store, &appended, partial, false);
        assert_eq!(
            store.trajectory_title(session).unwrap().as_deref(),
            Some(name)
        );
        assert!(matches!(
            store.upsert_selected_session_document_with_name_coverage(
                &named,
                &observation,
                initial,
                None,
                true
            ),
            Err(super::StoreError::TrajectorySourceChanged)
        ));

        let local = TrajectoryDocument {
            derived_title: Some(name),
            local_opener: LocalOpenerEvidence::Current,
            ..partial
        };
        write_selected_catalog_document(&store, &appended, local, false);
        let withdrawn = TrajectoryDocument {
            local_opener: LocalOpenerEvidence::Withdrawn,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: Some(name),
            derived_title: None,
            title_is_verified: false,
            ..partial
        };
        assert_eq!(
            save_observed_title(&store, session, withdrawn).as_deref(),
            Some(name)
        );

        let verified = TrajectoryDocument {
            title_authoritative: true,
            ..local
        };
        write_selected_catalog_document(&store, &appended, verified, true);
        assert_eq!(
            store.trajectory_title(session).unwrap().as_deref(),
            Some(name)
        );
        assert_eq!(save_observed_title(&store, session, withdrawn), None);

        check_unattributed_visible_prefix(&store, initial, partial);
    }

    fn write_selected_catalog_document(
        store: &Store,
        row: &IndexedSession,
        doc: TrajectoryDocument<'_>,
        name_observed: bool,
    ) {
        let current = store.observe_selected_session(&row.session).unwrap();
        store
            .upsert_selected_session_document_with_name_coverage(
                row,
                &current,
                doc,
                None,
                name_observed,
            )
            .unwrap();
    }

    fn check_unattributed_visible_prefix(
        store: &Store,
        initial: TrajectoryDocument<'_>,
        partial: TrajectoryDocument<'_>,
    ) {
        let inherited = SessionRef::new(Provider::Codex, "inherited-only");
        save_observed_title(store, &inherited, initial);
        assert_eq!(
            save_observed_title(store, &inherited, partial).as_deref(),
            Some("inherited opener")
        );
        let empty = TrajectoryDocument {
            derived_title: None,
            title_is_verified: false,
            ..partial
        };
        assert_eq!(save_observed_title(store, &inherited, empty), None);
    }

    #[test]
    fn a_same_spelling_catalog_name_protects_against_a_stale_local_rollback() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let name = "continue";
        let time = Utc::now();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(51)),
            contents: Some(Uuid::from_u128(52)),
            ..Default::default()
        };
        let catalog = IndexedSession {
            updated_at: Some(time),
            source_revision: Some(revision),
            ..indexed_session(Provider::Codex, "named-local-opener", name)
        };
        let session = &catalog.session;
        let initial = TrajectoryDocument {
            local_opener: LocalOpenerEvidence::Current,
            title_authoritative: true,
            derived_title: Some(name),
            truncation_strategy: "none",
            ..rollout_document("original searchable content", time, revision, true, 6)
        };
        save_observed_title(&store, session, initial);
        let prior =
            super::TrajectorySourceGuard::Observed(store.trajectory_index_state(session).unwrap());
        store
            .refresh_codex_catalog(std::slice::from_ref(&catalog), true, true, &[])
            .unwrap();

        for full in [false, true] {
            let negative = TrajectoryDocument {
                local_opener: LocalOpenerEvidence::Withdrawn,
                database_step_proof: None,
                desktop_projection: None,
                legacy_utf8_text: None,
                withdrawn_title: Some(name),
                truncation_strategy: if full { "none" } else { "source_incomplete" },
                ..rollout_document("updated searchable content", time, revision, full, 6)
            };
            store
                .upsert_observed_trajectory_documents(&[(session, negative, prior)])
                .unwrap();
            assert_eq!(
                store.trajectory_title(session).unwrap().as_deref(),
                Some(name)
            );
            let term = if full { "updated" } else { "original" };
            assert_eq!(
                store
                    .search_session_trajectories(term, 2)
                    .unwrap()
                    .as_slice(),
                std::slice::from_ref(session)
            );
        }
    }

    #[test]
    fn a_current_preview_can_rename_a_fully_searchable_rollout() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "named-rollout");
        let time = Utc::now();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let write = |full, text: &'static str, title| {
            let doc = TrajectoryDocument {
                truncation_strategy: "none",
                derived_title: title,
                title_authoritative: true,
                ..rollout_document(text, time, revision, full, 5)
            };
            let prior = store.trajectory_index_state(&session).unwrap();
            store
                .upsert_observed_trajectory_documents(&[(
                    &session,
                    doc,
                    super::TrajectorySourceGuard::Observed(prior),
                )])
                .unwrap();
        };
        write(true, "opening middle ending", Some("Original name"));
        for title in [Some("Renamed"), None] {
            write(false, "opening ending", title);
            assert_eq!(store.trajectory_title(&session).unwrap().as_deref(), title);
            assert!(
                store
                    .trajectory_index_state(&session)
                    .unwrap()
                    .unwrap()
                    .source_complete
            );
            assert_eq!(
                store
                    .search_session_trajectories("middle", 3)
                    .unwrap()
                    .as_slice(),
                std::slice::from_ref(&session)
            );
        }
    }

    fn save_observed_title(
        store: &Store,
        session: &SessionRef,
        document: TrajectoryDocument<'_>,
    ) -> Option<String> {
        let observed = store.trajectory_index_state(session).unwrap();
        store
            .upsert_observed_trajectory_documents(&[(
                session,
                document,
                super::TrajectorySourceGuard::Observed(observed),
            )])
            .unwrap();
        store.trajectory_title(session).unwrap()
    }

    #[test]
    fn the_title_upgrade_preserves_a_stamped_v6_local_opener() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(21)),
            contents: Some(Uuid::from_u128(22)),
            ..SourceRevision::default()
        };
        let title = Some("Original question");
        for version in [5, 6] {
            let session = SessionRef::new(Provider::Codex, version.to_string());
            let doc = TrajectoryDocument {
                derived_title: title,
                local_opener: LocalOpenerEvidence::Current,
                ..rollout_document("visible text", Utc::now(), revision, true, version)
            };
            assert_eq!(save_observed_title(&store, &session, doc).as_deref(), title);
        }
        store.connection.borrow().execute(
            "DELETE FROM session_trajectory_migrations WHERE name = 'reverify_legacy_codex_titles'", [],
        ).unwrap();
        drop(store);
        let store = Store::open(path).unwrap();
        for (version, expected) in [("5", None), ("6", title)] {
            let session = SessionRef::new(Provider::Codex, version);
            assert_eq!(
                store.trajectory_title(&session).unwrap().as_deref(),
                expected
            );
        }
    }

    #[test]
    fn a_sampled_rollback_can_replace_only_a_verified_local_opener() {
        use super::LocalOpenerEvidence::{Current, Unknown, Withdrawn};

        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let save = |session, document| save_observed_title(&store, session, document);
        let ordinary = SessionRef::new(Provider::Codex, "ordinary");
        let named = SessionRef::new(Provider::Codex, "named");
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(42)),
            contents: Some(Uuid::from_u128(43)),
            ..SourceRevision::default()
        };
        let title = Some("continue");
        let initial = TrajectoryDocument {
            derived_title: title,
            local_opener: Current,
            truncation_strategy: "none",
            ..rollout_document("complete searchable body", Utc::now(), revision, true, 6)
        };
        for session in [&ordinary, &named] {
            assert_eq!(save(session, initial).as_deref(), title);
        }
        let provider = TrajectoryDocument {
            local_opener: Unknown,
            title_is_verified: true,
            ..initial
        };
        assert_eq!(save(&named, provider).as_deref(), title);
        // An ordinary full read can see the same opening text after the catalog stops supplying a name.
        assert_eq!(save(&named, initial).as_deref(), title);

        let preview = TrajectoryDocument {
            local_opener: Current,
            derived_title: Some("replacement"),
            ..rollout_document(
                "preview only",
                initial.source_updated_at,
                revision,
                false,
                6,
            )
        };
        assert_eq!(save(&ordinary, preview).as_deref(), Some("replacement"));
        assert_eq!(save(&named, preview).as_deref(), title);
        assert!(
            store
                .search_session_trajectories("complete", 5)
                .unwrap()
                .contains(&ordinary)
        );
        let negative = TrajectoryDocument {
            local_opener: Withdrawn,
            derived_title: None,
            withdrawn_title: title,
            ..preview
        };
        assert_eq!(save(&ordinary, negative).as_deref(), Some("replacement"));
        assert_eq!(save(&named, negative).as_deref(), title);
        assert!(
            save(
                &ordinary,
                TrajectoryDocument {
                    withdrawn_title: Some("replacement"),
                    ..negative
                }
            )
            .is_none()
        );
        assert_eq!(save(&ordinary, initial).as_deref(), title);
        assert!(
            save(
                &ordinary,
                TrajectoryDocument {
                    derived_title: None,
                    ..preview
                }
            )
            .is_none()
        );
    }

    #[test]
    fn locally_withdrawn_title_changes_only_the_matching_message_title() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "rollback");
        let file = Uuid::from_u128(42);
        let revision = SourceRevision {
            file: Some(file),
            contents: Some(Uuid::from_u128(43)),
            ..SourceRevision::default()
        };
        let doc = rollout_document("surviving text", Utc::now(), revision, true, 5);
        let save = |doc| save_observed_title(&store, &session, doc);
        let title = Some("Local request");
        let initial = TrajectoryDocument {
            derived_title: title,
            ..doc
        };
        // An older writer could save this local opener before it knew the parent history.
        store
            .upsert_trajectory_document(&session, &initial)
            .unwrap();
        assert_eq!(save(initial).as_deref(), title);
        assert_eq!(
            store
                .withdrawable_trajectory_title(&session, file, false)
                .unwrap()
                .as_deref(),
            title
        );
        assert!(
            store
                .withdrawable_trajectory_title(&session, Uuid::nil(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            save(TrajectoryDocument {
                withdrawn_title: Some("Another request"),
                ..doc
            })
            .as_deref(),
            title
        );
        let preview = rollout_document("preview only", doc.source_updated_at, revision, false, 5);
        assert_eq!(save(preview).as_deref(), title);
        assert!(
            save(TrajectoryDocument {
                withdrawn_title: title,
                ..preview
            })
            .is_none()
        );
        assert_eq!(
            store.search_session_trajectories("surviving", 1).unwrap(),
            std::slice::from_ref(&session)
        );

        // The provider can later name the session with exactly the same words as its first request.
        let named = TrajectoryDocument {
            title_is_verified: true,
            ..initial
        };
        assert_eq!(save(named).as_deref(), title);
        assert!(
            store
                .withdrawable_trajectory_title(&session, file, false)
                .unwrap()
                .is_none()
        );
        let later = TrajectoryDocument {
            derived_title: Some("Another local request"),
            withdrawn_title: title,
            source_updated_at: doc.source_updated_at + chrono::Duration::seconds(5),
            ..doc
        };
        assert_eq!(save(later).as_deref(), title);
        assert_eq!(
            save(TrajectoryDocument {
                withdrawn_title: title,
                ..doc
            })
            .as_deref(),
            title
        );
    }

    #[test]
    fn missing_parent_keeps_its_verified_opener_through_an_identical_local_rollback() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "inherited");
        let file = Uuid::from_u128(84);
        let revision = SourceRevision {
            file: Some(file),
            ..SourceRevision::default()
        };
        let base = rollout_document("local conversation", Utc::now(), revision, true, 5);
        let title = Some("continue");
        let save = |doc| {
            let guard = super::TrajectorySourceGuard::Observed(
                store.trajectory_index_state(&session).unwrap(),
            );
            store
                .upsert_observed_trajectory_documents(&[(&session, doc, guard)])
                .unwrap();
        };
        save(TrajectoryDocument {
            truncation_strategy: "none",
            title_is_verified: true,
            local_opener: LocalOpenerEvidence::Unknown,
            derived_title: title,
            ..base
        });
        let incomplete = TrajectoryDocument {
            source_revision: SourceRevision {
                contents: Some(Uuid::from_u128(85)),
                ..revision
            },
            ..base
        };
        save(TrajectoryDocument {
            derived_title: title,
            ..incomplete
        });
        assert_eq!(
            store
                .trajectory_index_state(&session)
                .unwrap()
                .unwrap()
                .source_revision,
            incomplete.source_revision
        );
        assert!(
            store
                .withdrawable_trajectory_title(&session, file, false)
                .unwrap()
                .is_none()
        );
        save(TrajectoryDocument {
            withdrawn_title: title,
            ..incomplete
        });
        assert_eq!(store.trajectory_title(&session).unwrap().as_deref(), title);
        save(TrajectoryDocument {
            truncation_strategy: "none",
            title_authoritative: true,
            ..incomplete
        });
        assert!(store.trajectory_title(&session).unwrap().is_none());
    }

    fn migrate_old_title_store(store: Store, path: &std::path::Path) -> Store {
        drop(store);
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(
                "ALTER TABLE session_trajectories DROP COLUMN derived_title_is_local_opener;
                 ALTER TABLE session_trajectories DROP COLUMN derived_title_is_verified;",
            )
            .unwrap();
        Store::open(path).unwrap()
    }

    fn seed_legacy_title_catalog(store: &Store, revision: SourceRevision, catalog_title: &str) {
        for (id, title) in [("named", "Saved request"), ("formatted", catalog_title)] {
            store
                .upsert_indexed_session(&IndexedSession {
                    source_revision: Some(revision),
                    ..indexed_session(Provider::Codex, id, title)
                })
                .unwrap();
        }
    }

    fn old_explicit_title(
        store: &Store,
        id: &str,
        file: Uuid,
        name: Option<&str>,
    ) -> (IndexedSession, TrajectoryDocument<'static>) {
        let revision = SourceRevision {
            file: Some(file),
            contents: Some(Uuid::from_u128(100)),
            ..SourceRevision::default()
        };
        let catalog = IndexedSession {
            source_revision: Some(revision),
            title: name.map(str::to_owned),
            ..indexed_session(Provider::Codex, id, "")
        };
        store.upsert_indexed_session(&catalog).unwrap();
        let doc = TrajectoryDocument {
            title_is_verified: true,
            derived_title: Some("Old label"),
            ..rollout_document("complete source", Utc::now(), revision, true, 6)
        };
        save_observed_title(store, &catalog.session, doc);
        (catalog, doc)
    }

    fn reopen_with_legacy_title_proofs(store: Store, path: &std::path::Path) -> Store {
        store.connection.borrow().execute_batch("DROP TABLE codex_workspace_rechecks;
            CREATE TABLE codex_workspace_rechecks(trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                source_file_id BLOB NOT NULL,source_contents_id BLOB NOT NULL);").unwrap();
        drop(store);
        Store::open(path).unwrap()
    }

    #[test]
    fn a_deferred_legacy_rollback_retries_only_the_same_rollout_in_its_own_home() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let (old, doc) = old_explicit_title(&store, "deferred", Uuid::from_u128(77), None);
        let store = reopen_with_legacy_title_proofs(store, &path);
        let appended = SourceRevision {
            contents: Some(Uuid::from_u128(201)),
            ..doc.source_revision
        };
        store
            .upsert_indexed_session(&IndexedSession {
                source_revision: Some(appended),
                ..old.clone()
            })
            .unwrap();
        let pending = TrajectoryDocument {
            withdrawn_title: doc.derived_title,
            local_opener: LocalOpenerEvidence::DeferredLegacyName,
            ..rollout_document("appended searchable reply", Utc::now(), appended, false, 6)
        };
        assert_eq!(
            save_observed_title(&store, &old.session, pending).as_deref(),
            doc.derived_title
        );
        let retries = store.legacy_codex_name_retries().unwrap();
        assert_eq!(retries.get(&old.session), Some(&appended));
        let other = Store::open(&path).unwrap();
        other
            .connection
            .borrow()
            .execute(
                "UPDATE current_codex_home SET home=home || '/sessions/other'",
                [],
            )
            .unwrap();
        assert!(other.legacy_codex_name_retries().unwrap().is_empty());
        assert!(
            other
                .legacy_codex_opener_candidate(&old.session, appended.file.unwrap())
                .unwrap()
                .is_none()
        );
        let checked = TrajectoryDocument {
            local_opener: LocalOpenerEvidence::RecheckedCurrent,
            derived_title: Some("Recovered opener"),
            ..pending
        };
        assert_eq!(
            save_observed_title(&store, &old.session, checked).as_deref(),
            checked.derived_title
        );
        assert!(store.legacy_codex_name_retries().unwrap().is_empty());
    }

    #[test]
    fn tool_catalog_upgrade_rechecks_partial_titles_without_weakening_older_proofs() {
        use LocalOpenerEvidence::{Current, Unknown};

        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let id = |name| SessionRef::new(Provider::Codex, name);
        let doc = TrajectoryDocument {
            title_is_verified: true,
            derived_title: Some("previous request"),
            truncation_strategy: "none",
            ..rollout_document("surviving reply", Utc::now(), revision, false, 6)
        };
        for (name, evidence, full, named, ancestor) in [
            ("local", Current, false, false, false),
            ("dormant-local", Current, false, false, false),
            ("dormant-unknown", Unknown, false, false, false),
            ("protected", Unknown, false, false, false),
            ("older-proof", Unknown, false, false, false),
            ("complete", Unknown, true, false, false),
            ("named", Unknown, false, true, false),
            ("fork", Unknown, false, false, true),
        ] {
            let session = id(name);
            let current = TrajectoryDocument {
                local_opener: evidence,
                source_complete: full,
                source_revision: SourceRevision {
                    missing_ancestor: ancestor.then(|| Uuid::from_u128(3)),
                    ..revision
                },
                ..doc
            };
            store
                .upsert_indexed_session(&IndexedSession {
                    title: named.then(|| "previous request".to_owned()),
                    source_revision: Some(current.source_revision),
                    ..indexed_session(Provider::Codex, name, "")
                })
                .unwrap();
            save_observed_title(&store, &session, current);
        }
        store.connection.borrow().execute_batch(
            "UPDATE session_index SET source_file_id=zeroblob(16),title='Other selected rollout'
             WHERE session_id IN ('dormant-local','dormant-unknown');
             ALTER TABLE legacy_codex_title_proofs DROP COLUMN requires_tool_catalog;
             INSERT INTO legacy_codex_title_proofs SELECT id,source_file_id,source_contents_id,derived_title
             FROM session_trajectories WHERE session_id='older-proof';"
        ).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let states = store.trajectory_index_states().unwrap();
        for name in ["local", "dormant-local", "protected", "older-proof"] {
            assert_eq!(states[&id(name)].document_version, 5);
        }
        for name in ["complete", "named", "fork", "dormant-unknown"] {
            assert_eq!(states[&id(name)].document_version, 6);
        }
        let proof = |name| {
            store
                .legacy_codex_opener_candidate(&id(name), revision.file.unwrap())
                .unwrap()
                .map(|p| p.requires_tool_catalog)
        };
        assert_eq!(proof("protected"), Some(true));
        assert_eq!(proof("complete"), Some(true));
        assert_eq!(proof("older-proof"), Some(false));
        assert_eq!(proof("named"), None);
        assert_eq!(proof("fork"), None);
        assert_eq!(proof("dormant-unknown"), None);
        let updated = TrajectoryDocument {
            local_opener: Current,
            derived_title: Some("surviving request"),
            ..doc
        };
        save_observed_title(&store, &id("local"), updated);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .trajectory_index_state(&id("local"))
                .unwrap()
                .unwrap()
                .document_version,
            6
        );
    }

    #[test]
    fn old_protected_titles_need_a_one_time_proof_from_the_same_unnamed_rollout() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let (old, doc) = old_explicit_title(&store, "old-explicit", Uuid::from_u128(1), None);
        let (raced, raced_doc) = old_explicit_title(&store, "raced", Uuid::from_u128(2), None);
        let (named, _) = old_explicit_title(
            &store,
            "already-named",
            Uuid::from_u128(3),
            Some("Old label"),
        );
        let store = reopen_with_legacy_title_proofs(store, &path);
        let candidate = |catalog: &IndexedSession| {
            store
                .legacy_codex_opener_candidate(
                    &catalog.session,
                    catalog.source_revision.unwrap().file.unwrap(),
                )
                .unwrap()
        };
        assert_eq!(
            candidate(&old).as_ref().map(|c| c.title.as_str()),
            doc.derived_title
        );
        assert_eq!(
            candidate(&raced).as_ref().map(|c| c.title.as_str()),
            raced_doc.derived_title
        );
        assert!(candidate(&named).is_none());
        let append = SourceRevision {
            contents: Some(Uuid::from_u128(101)),
            ..doc.source_revision
        };
        store
            .upsert_indexed_session(&IndexedSession {
                source_revision: Some(append),
                ..old.clone()
            })
            .unwrap();
        let current = TrajectoryDocument {
            derived_title: Some("New opener"),
            withdrawn_title: doc.derived_title,
            local_opener: LocalOpenerEvidence::RecheckedCurrent,
            ..rollout_document("surviving reply", Utc::now(), append, false, 6)
        };
        assert_eq!(
            save_observed_title(&store, &old.session, current).as_deref(),
            current.derived_title
        );
        assert!(candidate(&old).is_none());
        let guard = super::TrajectorySourceGuard::Observed(
            store.trajectory_index_state(&raced.session).unwrap(),
        );
        store
            .upsert_indexed_session(&IndexedSession {
                title: Some("Old label".to_owned()),
                ..raced.clone()
            })
            .unwrap();
        store.upsert_indexed_session(&raced).unwrap();
        assert!(candidate(&raced).is_none());
        let stale = TrajectoryDocument {
            derived_title: None,
            withdrawn_title: raced_doc.derived_title,
            local_opener: LocalOpenerEvidence::RecheckedWithdrawn,
            ..raced_doc
        };
        assert!(matches!(
            store.upsert_observed_trajectory_documents(&[(&raced.session, stale, guard)]),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(
            store.trajectory_title(&raced.session).unwrap().as_deref(),
            raced_doc.derived_title
        );
    }

    #[test]
    fn upgrading_an_ambiguous_v6_title_retains_a_matching_provider_name() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let file = Uuid::from_u128(7);
        let revision = SourceRevision {
            file: Some(file),
            contents: Some(Uuid::from_u128(8)),
            ..SourceRevision::default()
        };
        let ordinary = SessionRef::new(Provider::Codex, "old-ordinary");
        let named = SessionRef::new(Provider::Codex, "old-named");
        let old = TrajectoryDocument {
            derived_title: Some("Old name"),
            title_is_verified: true,
            ..rollout_document("sampled reply", Utc::now(), revision, false, 6)
        };
        for session in [&ordinary, &named] {
            save_observed_title(&store, session, old);
        }
        store
            .upsert_indexed_session(&IndexedSession {
                source_revision: Some(revision),
                ..indexed_session(Provider::Codex, &named.id, "Old   name")
            })
            .unwrap();
        store
            .connection
            .borrow()
            .execute_batch(
                "ALTER TABLE session_trajectories DROP COLUMN derived_title_is_local_opener;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let selected = TrajectoryDocument {
            derived_title: Some("Surviving opener"),
            local_opener: LocalOpenerEvidence::Current,
            ..old
        };
        assert!(
            store
                .trajectory_index_state(&ordinary)
                .unwrap()
                .unwrap()
                .document_version
                < 6
        );
        assert_eq!(
            store
                .withdrawable_trajectory_title(&ordinary, file, true)
                .unwrap()
                .as_deref(),
            old.derived_title
        );
        assert!(
            store
                .withdrawable_trajectory_title(&named, file, true)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            save_observed_title(&store, &ordinary, selected).as_deref(),
            selected.derived_title
        );
        assert_eq!(
            save_observed_title(&store, &named, selected).as_deref(),
            old.derived_title
        );
    }

    #[test]
    fn rechecking_an_unstamped_v5_title_does_not_create_a_v4_projection() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let session = SessionRef::new(Provider::Codex, "unstamped");
        let time = Utc::now();
        let document = TrajectoryDocument {
            derived_title: Some("Saved request"),
            ..rollout_document("known text", time, SourceRevision::default(), true, 5)
        };
        store
            .upsert_trajectory_document(&session, &document)
            .unwrap();
        let store = migrate_old_title_store(store, &path);
        assert_eq!(
            store
                .trajectory_index_state(&session)
                .unwrap()
                .unwrap()
                .document_version,
            0
        );
        assert!(
            store
                .legacy_codex_projection_hash(&session, time)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn v5_projection_digest_requires_the_row_migrated_before_source_revisions() {
        use sha2::{Digest, Sha256};

        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let session = SessionRef::new(Provider::Codex, "pre-revisions");
        let time = DateTime::from_timestamp_millis(42).unwrap();
        let text = "known projection";
        let document = TrajectoryDocument {
            derived_title: Some("Saved request"),
            truncation_strategy: "none",
            ..rollout_document(text, time, SourceRevision::default(), true, 5)
        };
        store
            .upsert_trajectory_document(&session, &document)
            .unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "DROP TABLE legacy_codex_v5_projections;
             DELETE FROM session_trajectory_migrations
             WHERE name IN ('recheck_v5_codex_coverage', 'reverify_legacy_codex_titles');
             DROP TRIGGER detach_changed_legacy_locator; DROP TRIGGER detach_removed_legacy_locator;
             DROP TRIGGER clear_replaced_primary_legacy_text;
             ALTER TABLE session_trajectories DROP COLUMN source_file_id;
             ALTER TABLE session_trajectories DROP COLUMN source_contents_id;
             ALTER TABLE session_trajectories DROP COLUMN missing_ancestor_id;",
            )
            .unwrap();
        let store = migrate_old_title_store(store, &path);
        assert!(store.trajectory_title(&session).unwrap().is_none());
        let matches = store
            .search_session_trajectory_matches("projection", 1)
            .unwrap();
        assert!(matches[0].source_complete && !matches[0].complete);
        assert_eq!(
            store
                .trajectory_index_state(&session)
                .unwrap()
                .unwrap()
                .document_version,
            5
        );
        assert_eq!(
            store.legacy_codex_projection_hash(&session, time).unwrap(),
            Some(Sha256::digest(text.as_bytes()).into())
        );

        let later = SessionRef::new(Provider::Codex, "after-migration");
        store.upsert_trajectory_document(&later, &document).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(
            store
                .legacy_codex_projection_hash(&later, time)
                .unwrap()
                .is_none()
        );
        let changed_time = time + chrono::Duration::milliseconds(1);
        store
            .connection
            .borrow()
            .execute(
                "UPDATE session_trajectories SET source_updated_at=?1 WHERE session_id=?2",
                params![changed_time.timestamp_millis(), session.id],
            )
            .unwrap();
        assert!(
            store
                .legacy_codex_projection_hash(&session, changed_time)
                .unwrap()
                .is_none()
        );
        store.connection.borrow().execute("UPDATE session_trajectories SET source_updated_at=?1, content_hash=zeroblob(32) WHERE session_id=?2",
            params![time.timestamp_millis(), session.id]).unwrap();
        assert!(
            store
                .legacy_codex_projection_hash(&session, time)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upgrade_rechecks_only_cached_incomplete_codex_message_titles() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let file = Uuid::from_u128(7);
        let revision = SourceRevision {
            file: Some(file),
            ..SourceRevision::default()
        };
        let base = TrajectoryDocument {
            derived_title: Some("Saved request"),
            ..rollout_document("known text", Utc::now(), revision, true, 5)
        };
        let codex = |id| SessionRef::new(Provider::Codex, id);
        let [message, preview] = ["message", "preview"].map(codex);
        let [named, formatted] = ["named", "formatted"].map(codex);
        let [complete, bundle] = ["complete", "bundle"].map(codex);
        let catalog_title = format!("  Saved   label   {}", "word ".repeat(80));
        let display_title = omnis_core::compact_search_title(&catalog_title).unwrap();
        let claude = SessionRef::new(Provider::Claude, "unaffected");
        for (session, doc) in [
            (
                &preview,
                TrajectoryDocument {
                    source_complete: false,
                    ..base
                },
            ),
            (
                &bundle,
                TrajectoryDocument {
                    origin: SessionTrajectoryOrigin::ImportedBundle,
                    ..base
                },
            ),
            (
                &formatted,
                TrajectoryDocument {
                    derived_title: Some(&display_title),
                    ..base
                },
            ),
            (&message, base),
            (&named, base),
            (
                &complete,
                TrajectoryDocument {
                    truncation_strategy: "none",
                    ..base
                },
            ),
            (&claude, base),
        ] {
            store.upsert_trajectory_document(session, &doc).unwrap();
        }
        seed_legacy_title_catalog(&store, revision, &catalog_title);
        let store = migrate_old_title_store(store, &path);
        let states = store.trajectory_index_states().unwrap();
        for session in [&message, &preview] {
            assert_eq!(states[session].document_version, 4);
        }
        for session in [&named, &formatted, &complete, &claude, &bundle] {
            assert_eq!(states[session].document_version, 5);
        }
        for protected in [&named, &formatted, &complete] {
            assert!(
                store
                    .withdrawable_trajectory_title(protected, file, false)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            store.trajectory_title(&formatted).unwrap().as_deref(),
            Some(display_title.as_str())
        );
        store.upsert_trajectory_document(&message, &base).unwrap();
        drop(store);
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .trajectory_index_states()
                .unwrap()[&message]
                .document_version,
            5
        );
    }

    #[test]
    fn workspace_upgrade_reprojects_only_stamped_full_native_codex_documents_once() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let time = DateTime::from_timestamp_millis(1000).unwrap();
        let stamped = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let sessions = [
            (
                Provider::Codex,
                "full",
                stamped,
                true,
                SessionTrajectoryOrigin::Native,
                4,
            ),
            (
                Provider::Codex,
                "sample",
                stamped,
                false,
                SessionTrajectoryOrigin::Native,
                5,
            ),
            (
                Provider::Codex,
                "no-proof",
                SourceRevision::default(),
                true,
                SessionTrajectoryOrigin::Native,
                5,
            ),
            (
                Provider::Codex,
                "bundle",
                stamped,
                true,
                SessionTrajectoryOrigin::ImportedBundle,
                5,
            ),
            (
                Provider::Claude,
                "other",
                stamped,
                true,
                SessionTrajectoryOrigin::Native,
                5,
            ),
        ];
        for (provider, id, revision, full, origin, _) in sessions {
            let doc = TrajectoryDocument {
                origin,
                ..rollout_document("message", time, revision, full, 5)
            };
            store
                .upsert_trajectory_document(&SessionRef::new(provider, id), &doc)
                .unwrap();
        }
        store
            .connection
            .borrow()
            .execute_batch("DROP TABLE codex_indexed_workspaces")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        for (provider, id, _, _, _, version) in sessions {
            assert_eq!(
                store
                    .trajectory_index_state(&SessionRef::new(provider, id))
                    .unwrap()
                    .unwrap()
                    .document_version,
                version
            );
        }
        let recovered = SessionRef::new(Provider::Codex, "full");
        store
            .upsert_trajectory_document(
                &recovered,
                &rollout_document("reprojected", time, stamped, true, 5),
            )
            .unwrap();
        drop(store);
        assert_eq!(
            Store::open(path)
                .unwrap()
                .trajectory_index_state(&recovered)
                .unwrap()
                .unwrap()
                .document_version,
            5
        );
    }

    fn publish_workspace_fixture(
        store: &Store,
    ) -> (
        IndexedSession,
        super::CodexWorkspace,
        TrajectoryDocument<'static>,
    ) {
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let original = IndexedSession {
            source_revision: Some(revision),
            project_path: Some("header".into()),
            git_branch: Some("initial".into()),
            ..indexed_session(Provider::Codex, "session", "Visible session")
        };
        let session = &original.session;
        let time = DateTime::from_timestamp_millis(1_000).unwrap();
        let doc = rollout_document("verified reply", time, revision, true, 5);
        let verified = super::CodexWorkspace {
            source_revision: revision,
            project_path: Some("latest".into()),
            git_branch: None,
        };
        store
            .upsert_selected_session_document_with_workspace(
                &original,
                &store.observe_selected_session(session).unwrap(),
                doc,
                Some(&super::CodexWorkspaceUpdate::Verified(verified.clone())),
            )
            .unwrap();
        (original, verified, doc)
    }

    #[test]
    fn a_retained_full_document_accepts_only_its_verified_workspace_from_a_preview() {
        use super::CodexWorkspaceUpdate::Verified;

        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let (original, workspace, full) = publish_workspace_fixture(&store);
        let session = &original.session;
        let partial = TrajectoryDocument {
            derived_title: Some("Renamed"),
            title_is_verified: true,
            title_authoritative: true,
            ..rollout_document(
                "preview",
                full.source_updated_at,
                full.source_revision,
                false,
                full.document_version,
            )
        };
        let write = |route| {
            let observed = store.observe_selected_session(session).unwrap();
            let selected = IndexedSession {
                title: Some("Renamed".into()),
                ..observed.metadata.as_ref().unwrap().clone()
            };
            store.upsert_selected_session_document_with_workspace(
                &selected,
                &observed,
                partial,
                Some(&Verified(route)),
            )
        };
        for other in [
            super::CodexWorkspace {
                project_path: None,
                ..workspace.clone()
            },
            super::CodexWorkspace {
                git_branch: Some("other".into()),
                ..workspace.clone()
            },
        ] {
            assert!(matches!(
                write(other),
                Err(StoreError::TrajectorySourceChanged)
            ));
        }
        write(workspace.clone()).unwrap();
        assert_eq!(
            store.trajectory_title(session).unwrap().as_deref(),
            Some("Renamed")
        );
        assert_eq!(
            store
                .search_session_trajectories("verified reply", 2)
                .unwrap(),
            vec![session.clone()]
        );
        let connection = store.connection.borrow();
        connection.execute_batch(
            "INSERT INTO codex_workspace_rechecks SELECT trajectory_id,source_file_id,source_contents_id
             FROM codex_indexed_workspaces"
        ).unwrap();
        drop(connection);
        assert!(matches!(
            write(workspace.clone()),
            Err(StoreError::TrajectorySourceChanged)
        ));
        store
            .connection
            .borrow()
            .execute_batch(
                "DELETE FROM codex_workspace_rechecks; DELETE FROM codex_indexed_workspaces",
            )
            .unwrap();
        assert!(matches!(
            write(workspace),
            Err(StoreError::TrajectorySourceChanged)
        ));
    }

    #[test]
    fn stale_discovery_cannot_replace_a_verified_workspace_for_the_same_revision() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let (original, verified, _) = publish_workspace_fixture(&store);
        let session = &original.session;
        let refresh = |entry| {
            store
                .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(entry))
                .unwrap();
            store.indexed_session(session).unwrap().unwrap()
        };
        let changed = IndexedSession {
            source_revision: Some(SourceRevision {
                contents: Some(Uuid::from_u128(3)),
                ..verified.source_revision
            }),
            ..original.clone()
        };
        assert_eq!(refresh(&changed), changed);
        let resumed = refresh(&original);
        assert_eq!(resumed.project_path, verified.project_path);
        assert_eq!(resumed.git_branch, None);
    }

    #[test]
    fn only_a_matching_full_native_read_can_withdraw_a_workspace() {
        use super::{CodexWorkspaceUpdate, TrajectorySourceGuard};

        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let (original, verified, doc) = publish_workspace_fixture(&store);
        let session = &original.session;
        let revision = verified.source_revision;
        let refresh = || {
            store
                .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&original))
                .unwrap();
            store.indexed_session(session).unwrap().unwrap()
        };
        let guard = TrajectorySourceGuard::Observed(store.trajectory_index_state(session).unwrap());
        let write = |document, guard, update| {
            store.upsert_observed_trajectory_documents_with_workspaces(&[(
                session, document, guard, update,
            )])
        };
        let partial = TrajectoryDocument {
            source_complete: false,
            ..doc
        };
        write(partial, guard, None).unwrap();
        assert_eq!(refresh().project_path, verified.project_path);
        let withdraw = CodexWorkspaceUpdate::Withdrawn(revision);
        let other_file = SourceRevision {
            file: Some(Uuid::from_u128(4)),
            ..revision
        };
        let other_contents = SourceRevision {
            contents: Some(Uuid::from_u128(3)),
            ..revision
        };
        let mismatched = [other_file, other_contents].map(CodexWorkspaceUpdate::Withdrawn);
        for update in &mismatched {
            assert!(matches!(
                write(doc, guard, Some(update)),
                Err(StoreError::TrajectorySourceChanged)
            ));
        }
        for (document, rejected_guard) in [
            (partial, guard),
            (
                TrajectoryDocument {
                    origin: SessionTrajectoryOrigin::ImportedBundle,
                    ..doc
                },
                guard,
            ),
            (doc, TrajectorySourceGuard::NewerSource),
            (
                TrajectoryDocument {
                    source_revision: other_file,
                    ..doc
                },
                TrajectorySourceGuard::Observed(None),
            ),
        ] {
            assert!(matches!(
                write(document, rejected_guard, Some(&withdraw)),
                Err(StoreError::TrajectorySourceChanged)
            ));
        }
        assert_eq!(refresh().project_path, verified.project_path);
        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TRIGGER reject_workspace_clear BEFORE UPDATE ON codex_indexed_workspaces
             BEGIN SELECT RAISE(ABORT,'failed workspace write'); END;",
            )
            .unwrap();
        assert!(write(doc, guard, Some(&withdraw)).is_err());
        store
            .connection
            .borrow()
            .execute_batch("DROP TRIGGER reject_workspace_clear")
            .unwrap();
        assert_eq!(
            store
                .indexed_session(session)
                .unwrap()
                .unwrap()
                .project_path,
            verified.project_path
        );
        assert_eq!(refresh().project_path, verified.project_path);

        write(doc, guard, Some(&withdraw)).unwrap();
        let cleared = store.indexed_session(session).unwrap().unwrap();
        assert_eq!(cleared.project_path, None);
        assert_eq!(cleared.git_branch, None);
        assert_eq!(refresh(), cleared);
    }

    #[test]
    fn selected_damaged_read_cannot_republish_its_previous_workspace() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let (original, verified, doc) = publish_workspace_fixture(&store);
        let observed = store.observe_selected_session(&original.session).unwrap();
        let mut selected = observed.metadata.as_ref().unwrap().clone();
        selected.event_count += 1;
        store
            .upsert_selected_session_document_with_workspace(
                &selected,
                &observed,
                doc,
                Some(&super::CodexWorkspaceUpdate::Withdrawn(
                    verified.source_revision,
                )),
            )
            .unwrap();
        let saved = store.indexed_session(&selected.session).unwrap().unwrap();
        assert_eq!(saved.event_count, selected.event_count);
        assert_eq!(saved.project_path, None);
        assert_eq!(saved.git_branch, None);
    }

    #[test]
    fn an_unknown_workspace_survives_discovery_until_a_repaired_rollout_is_read() {
        use super::{CodexWorkspace, CodexWorkspaceUpdate};

        let directory = tempdir().unwrap();
        let database = directory.path().join("store.sqlite3");
        let store = Store::open(&database).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(7)),
            contents: Some(Uuid::from_u128(8)),
            ..SourceRevision::default()
        };
        let mut header = IndexedSession {
            source_revision: Some(revision),
            project_path: Some("creation".into()),
            git_branch: Some("original".to_owned()),
            ..indexed_session(Provider::Codex, "damaged", "Session title")
        };
        let time = DateTime::from_timestamp_millis(1_000).unwrap();
        let document = rollout_document("answer", time, revision, true, 6);
        store
            .upsert_selected_session_document_with_workspace(
                &header,
                &store.observe_selected_session(&header.session).unwrap(),
                document,
                Some(&CodexWorkspaceUpdate::Withdrawn(revision)),
            )
            .unwrap();
        drop(store);

        let store = Store::open(&database).unwrap();
        store.upsert_indexed_session(&header).unwrap();
        let saved = store.indexed_session(&header.session).unwrap().unwrap();
        assert_eq!(saved.source_revision, Some(revision));
        assert_eq!((saved.project_path, saved.git_branch), (None, None));

        let repaired = SourceRevision {
            contents: Some(Uuid::from_u128(9)),
            ..revision
        };
        header.source_revision = Some(repaired);
        let workspace = CodexWorkspace {
            source_revision: repaired,
            project_path: Some("latest".into()),
            git_branch: Some("current".to_owned()),
        };
        store
            .upsert_selected_session_document_with_workspace(
                &header,
                &store.observe_selected_session(&header.session).unwrap(),
                TrajectoryDocument {
                    source_revision: repaired,
                    ..document
                },
                Some(&CodexWorkspaceUpdate::Verified(workspace.clone())),
            )
            .unwrap();
        store.upsert_indexed_session(&header).unwrap();
        let saved = store.indexed_session(&header.session).unwrap().unwrap();
        assert_eq!(
            (saved.project_path, saved.git_branch),
            (workspace.project_path, workspace.git_branch)
        );
    }

    #[test]
    fn cached_search_pairs_unstamped_catalog_metadata_with_its_document_workspace() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let (header, workspace, doc) = publish_workspace_fixture(&store);
        let write_legacy_header = || {
            store.connection.borrow().execute_batch("UPDATE session_index
                SET source_file_id=NULL,source_contents_id=NULL,project_path='legacy header',git_branch='legacy branch'").unwrap();
        };
        let cached_scope = |provider| {
            let session = store
                .cached_sessions_with_titles(provider)
                .unwrap()
                .0
                .pop()
                .unwrap();
            (session.project_path, session.git_branch)
        };
        write_legacy_header();
        assert_eq!(cached_scope(None), (workspace.project_path.clone(), None));
        let legacy = store.indexed_session(&header.session).unwrap().unwrap();
        assert_eq!(
            legacy.project_path.as_deref(),
            Some(std::path::Path::new("legacy header"))
        );
        store
            .upsert_selected_session_document_with_workspace(
                &header,
                &store.observe_selected_session(&header.session).unwrap(),
                doc,
                Some(&super::CodexWorkspaceUpdate::Withdrawn(
                    workspace.source_revision,
                )),
            )
            .unwrap();
        write_legacy_header();
        assert_eq!(cached_scope(Some(Provider::Codex)), (None, None));
        let replacement = IndexedSession {
            source_revision: Some(SourceRevision {
                file: Some(Uuid::from_u128(121)),
                ..workspace.source_revision
            }),
            ..header
        };
        store.upsert_indexed_session(&replacement).unwrap();
        assert_eq!(
            cached_scope(None),
            (replacement.project_path, replacement.git_branch)
        );
    }

    #[test]
    fn an_old_sample_stays_unscoped_until_its_exact_workspace_is_verified() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("store.sqlite3");
        let store = Store::open(&database).unwrap();
        let (header, workspace, doc) = publish_workspace_fixture(&store);
        let revision = workspace.source_revision;
        let file = revision.file.unwrap();
        store
            .connection
            .borrow()
            .execute_batch(
                "UPDATE session_trajectories SET document_version=6,source_complete=0,complete=0;
             DROP TABLE codex_workspace_rechecks;",
            )
            .unwrap();
        let index =
            super::CodexWorkspace::discovery_index_from(&store.connection.borrow(), Some(file))
                .unwrap();
        assert!(index.verified.is_empty());
        assert!(index.pending.contains(&revision));
        assert!(
            super::CodexWorkspace::discovery_index_from(
                &store.connection.borrow(),
                Some(Uuid::from_u128(99))
            )
            .unwrap()
            .pending
            .is_empty()
        );
        drop(store);

        let store = Store::open(&database).unwrap();
        let state = store
            .trajectory_index_state(&header.session)
            .unwrap()
            .unwrap();
        assert_eq!(state.document_version, 5);
        assert_eq!(state.source_revision, revision);
        assert!(!state.source_complete);
        store.upsert_indexed_session(&header).unwrap();
        let catalog = store.indexed_session(&header.session).unwrap().unwrap();
        assert_eq!((catalog.project_path, catalog.git_branch), (None, None));

        let replacement = IndexedSession {
            source_revision: Some(SourceRevision {
                contents: Some(Uuid::from_u128(42)),
                ..revision
            }),
            ..header.clone()
        };
        store.upsert_indexed_session(&replacement).unwrap();
        assert_eq!(
            store.indexed_session(&header.session).unwrap(),
            Some(replacement)
        );
        store.upsert_indexed_session(&header).unwrap();
        store
            .upsert_selected_session_document_with_workspace(
                &header,
                &store.observe_selected_session(&header.session).unwrap(),
                TrajectoryDocument {
                    source_complete: false,
                    document_version: 6,
                    ..doc
                },
                Some(&super::CodexWorkspaceUpdate::Verified(workspace.clone())),
            )
            .unwrap();
        drop(store);

        let store = Store::open(&database).unwrap();
        let refreshed =
            super::CodexWorkspace::discovery_index_from(&store.connection.borrow(), Some(file))
                .unwrap();
        assert!(refreshed.pending.is_empty());
        assert_eq!(
            refreshed.verified[&file].project_path,
            workspace.project_path
        );
        let state = store
            .trajectory_index_state(&header.session)
            .unwrap()
            .unwrap();
        assert_eq!(state.document_version, 6);
        assert!(!state.source_complete);
    }

    #[test]
    fn settings_recheck_retains_each_unverified_file_and_contents_pair() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let (header, workspace, doc) = publish_workspace_fixture(&store);
        let current = workspace.source_revision;
        let old = SourceRevision {
            contents: Some(Uuid::from_u128(74)),
            ..current
        };
        let archived = SourceRevision {
            file: Some(Uuid::from_u128(75)),
            ..current
        };
        let expected = std::collections::HashSet::from([current, old, archived]);
        {
            let connection = store.connection.borrow();
            connection.execute_batch("DROP TABLE codex_workspace_rechecks;
                CREATE TABLE codex_workspace_rechecks(trajectory_id INTEGER PRIMARY KEY,source_file_id BLOB NOT NULL,source_contents_id BLOB NOT NULL);
                CREATE INDEX codex_workspace_rechecks_source ON codex_workspace_rechecks(source_file_id,source_contents_id);
                UPDATE session_trajectories SET document_version=6;").unwrap();
            connection.execute("INSERT INTO codex_workspace_rechecks SELECT id,?1,source_contents_id FROM session_trajectories",
                [archived.file.unwrap().into_bytes()]).unwrap();
            connection
                .execute(
                    "UPDATE codex_indexed_workspaces SET source_contents_id=?1",
                    [old.contents.unwrap().into_bytes()],
                )
                .unwrap();
            let before = super::CodexWorkspace::discovery_index_from(&connection, None).unwrap();
            assert!(before.verified.is_empty());
            assert_eq!(before.pending, expected);
        }
        drop(store);
        let store = Store::open(&path).unwrap();
        let migrated =
            super::CodexWorkspace::discovery_index_from(&store.connection.borrow(), None).unwrap();
        assert_eq!(migrated.pending, expected);
        assert!(migrated.verified.is_empty());
        let state = store
            .trajectory_index_state(&header.session)
            .unwrap()
            .unwrap();
        assert_eq!(state.document_version, 5);
        assert!(state.source_complete);
        assert_eq!(state.source_revision, current);
        store.upsert_indexed_session(&header).unwrap();
        assert!(
            store
                .indexed_session(&header.session)
                .unwrap()
                .unwrap()
                .project_path
                .is_none()
        );
        store
            .upsert_selected_session_document_with_workspace(
                &header,
                &store.observe_selected_session(&header.session).unwrap(),
                TrajectoryDocument {
                    document_version: 6,
                    ..doc
                },
                Some(&super::CodexWorkspaceUpdate::Verified(workspace.clone())),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let result =
            super::CodexWorkspace::discovery_index_from(&store.connection.borrow(), None).unwrap();
        assert_eq!(
            result.pending,
            std::collections::HashSet::from([old, archived])
        );
        assert_eq!(
            result.verified[&current.file.unwrap()].project_path,
            workspace.project_path
        );
        assert_eq!(
            store
                .trajectory_index_state(&header.session)
                .unwrap()
                .unwrap()
                .document_version,
            6
        );
    }

    #[test]
    fn v6_recheck_preserves_full_read_budgets_and_discards_uncertain_workspaces() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("store.sqlite3");
        let store = Store::open(&database).unwrap();
        let time = DateTime::from_timestamp_millis(42).unwrap();
        let named = Some("Provider name");
        let cases = [
            ("sparse", false, "source_incomplete", None, None, 5, false),
            ("damaged", true, "source_incomplete", None, None, 5, false),
            ("complete", true, "none", None, None, 6, true),
            ("opener", true, "none", Some("Local opener"), None, 5, true),
            ("clipped", true, "document_head_tail", named, named, 6, true),
        ];
        for (i, (id, full, strategy, title, name, _, _)) in cases.iter().copied().enumerate() {
            let revision = SourceRevision {
                file: Some(Uuid::from_u128(i as u128 + 1)),
                contents: Some(Uuid::from_u128(100)),
                ..SourceRevision::default()
            };
            let selected = IndexedSession {
                title: name.map(str::to_owned),
                source_revision: Some(revision),
                ..indexed_session(Provider::Codex, id, "")
            };
            let mut doc = TrajectoryDocument {
                truncation_strategy: strategy,
                derived_title: title,
                title_is_verified: title.is_some(),
                ..rollout_document("searchable body", time, revision, full, 6)
            };
            if strategy == "document_head_tail" {
                doc.source_byte_count += 1_000;
            }
            let workspace = super::CodexWorkspace {
                source_revision: revision,
                project_path: Some("turn-directory".into()),
                git_branch: None,
            };
            store
                .upsert_selected_session_document_with_workspace(
                    &selected,
                    &store.observe_selected_session(&selected.session).unwrap(),
                    doc,
                    Some(&super::CodexWorkspaceUpdate::Verified(workspace)),
                )
                .unwrap();
        }
        store
            .connection
            .borrow()
            .execute_batch(
                "ALTER TABLE session_trajectories DROP COLUMN derived_title_is_local_opener;",
            )
            .unwrap();
        drop(store);

        let store = Store::open(&database).unwrap();
        for (id, full, _, _, _, expected_version, keep_workspace) in cases {
            let state = store
                .trajectory_index_state(&SessionRef::new(Provider::Codex, id))
                .unwrap()
                .unwrap();
            assert_eq!(
                (state.document_version, state.source_complete),
                (expected_version, full),
                "{id}"
            );
            assert!(
                state.source_revision.file.is_some() && state.source_revision.contents.is_some()
            );
            let retained: bool = store.connection.borrow().query_row(
                "SELECT EXISTS (SELECT 1 FROM codex_indexed_workspaces w JOIN session_trajectories t
                 ON t.id=w.trajectory_id WHERE t.provider='codex' AND t.session_id=?1)",
                [id], |row| row.get(0),
            ).unwrap();
            assert_eq!(retained, keep_workspace, "{id}");
        }
        store
            .connection
            .borrow()
            .execute(
                "UPDATE session_trajectories SET document_version=6 WHERE session_id='sparse'",
                [],
            )
            .unwrap();
        drop(store);
        let reopened = Store::open(&database)
            .unwrap()
            .trajectory_index_states()
            .unwrap();
        assert_eq!(
            reopened[&SessionRef::new(Provider::Codex, "sparse")].document_version,
            6
        );
    }

    #[test]
    fn an_unavailable_rollout_keeps_search_content_protected_by_a_bundle() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let time = DateTime::from_timestamp_millis(1000).unwrap();
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            ..SourceRevision::default()
        };
        let row = IndexedSession {
            source_revision: Some(revision),
            updated_at: Some(time),
            ..indexed_session(Provider::Codex, "shared", "Shared")
        };
        store.upsert_indexed_session(&row).unwrap();
        let imported = TrajectoryDocument {
            origin: SessionTrajectoryOrigin::ImportedBundle,
            ..rollout_document(
                "imported evidence",
                time - chrono::Duration::seconds(1),
                revision,
                true,
                5,
            )
        };
        store
            .upsert_trajectory_document(&row.session, &imported)
            .unwrap();
        let native = rollout_document("native evidence", time, revision, true, 5);
        store
            .upsert_trajectory_document(&row.session, &native)
            .unwrap();
        store
            .remove_unavailable_codex_sessions(std::slice::from_ref(&row))
            .unwrap();
        assert!(store.indexed_session(&row.session).unwrap().is_none());
        assert_eq!(
            store
                .search_session_trajectories("native evidence", 10)
                .unwrap(),
            [row.session]
        );
    }

    #[test]
    fn selected_metadata_and_document_reject_obsolete_observations_together() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let revision = |id| SourceRevision {
            file: Some(Uuid::from_u128(id)),
            ..SourceRevision::default()
        };
        let archive = IndexedSession {
            source_revision: Some(revision(1)),
            ..indexed_session(Provider::Codex, "recovered", "Archive title")
        };
        let active = IndexedSession {
            source_revision: Some(revision(2)),
            title: Some("Active title".to_owned()),
            project_path: Some("active-workspace".into()),
            ..archive.clone()
        };
        let session = &active.session;
        let time = DateTime::from_timestamp_millis(1_000).unwrap();
        let archive_doc = rollout_document("archiveword", time, revision(1), true, 5);
        let active_doc = rollout_document("activeword", time, revision(2), true, 5);
        let observe = || store.observe_selected_session(session).unwrap();
        let write = |selected: &IndexedSession,
                     observed: &super::SelectedSessionObservation,
                     doc: TrajectoryDocument<'_>| {
            store.upsert_selected_session_document(selected, observed, doc)
        };
        write(&archive, &observe(), archive_doc).unwrap();
        let old = observe();
        store
            .upsert_indexed_session(&IndexedSession {
                title: Some("Concurrent discovery".into()),
                ..active.clone()
            })
            .unwrap();
        assert!(matches!(
            write(&active, &old, active_doc),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(observe().state, old.state);
        write(&active, &observe(), active_doc).unwrap();
        store
            .remove_unavailable_codex_sessions(std::slice::from_ref(&archive))
            .unwrap();
        assert_eq!(observe().metadata.as_ref(), Some(&active));
        assert!(matches!(
            write(&archive, &old, archive_doc),
            Err(StoreError::TrajectorySourceChanged)
        ));
        let before_failure = observe();
        let failure = super::TrajectoryIndexFailure {
            source_time_precise: true,
            native_summary: false,
            source_updated_at: Some(time),
            source_file: revision(1).file,
            source_contents: revision(1).contents,
            document_version: 5,
        };
        store
            .record_trajectory_index_failure(session, &failure)
            .unwrap();
        assert!(matches!(
            write(&active, &before_failure, active_doc),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert_eq!(observe().failure, Some(failure));
        write(&active, &observe(), active_doc).unwrap();
        assert_eq!(observe().failure, None);
        let same_stamp = observe();
        let replacement = rollout_document("replacementword", time, revision(2), true, 5);
        write(&active, &same_stamp, replacement).unwrap();
        assert!(matches!(
            write(&active, &same_stamp, active_doc),
            Err(StoreError::TrajectorySourceChanged)
        ));
        store
            .connection
            .borrow()
            .execute_batch(
                "CREATE TRIGGER reject_metadata BEFORE UPDATE ON session_index
             BEGIN SELECT RAISE(ABORT, 'synthetic metadata write failure'); END",
            )
            .unwrap();
        assert!(write(&archive, &observe(), archive_doc).is_err());
        assert_eq!(
            store.indexed_session(session).unwrap(),
            Some(active.clone())
        );
        assert_eq!(
            store
                .search_session_trajectories("replacementword", 1)
                .unwrap(),
            std::slice::from_ref(session)
        );
        assert!(
            store
                .search_session_trajectories("archiveword", 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn selected_rollout_replaces_another_file_and_recovers_its_missing_ancestor() {
        let directory = tempdir().expect("temporary directory");
        let store = Store::open(directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "reverted");
        let newer = DateTime::from_timestamp_millis(2000).expect("newer time");
        let older = DateTime::from_timestamp_millis(1000).expect("older time");
        let first = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: None,
            missing_ancestor: None,
        };
        let second = SourceRevision {
            file: Some(Uuid::from_u128(2)),
            contents: None,
            missing_ancestor: Some(Uuid::from_u128(3)),
        };
        let observed_write = |doc, prior| {
            store.upsert_observed_trajectory_documents(&[(
                &session,
                doc,
                super::TrajectorySourceGuard::Observed(prior),
            )])
        };
        let write = |text, time, revision, full, version| {
            let prior = store.trajectory_index_state(&session).expect("prior state");
            observed_write(rollout_document(text, time, revision, full, version), prior)
                .expect("write document");
        };
        let current = |time, revision| {
            store
                .session_trajectory_source_revision_is_current(&session, time, revision)
                .expect("check selected source")
        };
        write("retired branch", newer, first, true, 2);
        assert!(!current(newer, second));
        write("old indexer", older, second, true, 1);
        assert!(current(newer, first));
        write("restored branch", older, second, false, 2);
        assert!(!current(older, second));
        assert!(
            store
                .search_session_trajectories("retired", 10)
                .expect("search")
                .is_empty()
        );
        write("restored branch", older, second, true, 2);
        assert!(current(older, second));

        let recovered = SourceRevision {
            missing_ancestor: None,
            ..second
        };
        assert!(!current(older, recovered));
        let before_recovery = store.trajectory_index_state(&session).expect("prior state");
        write("restored branch", older, recovered, false, 2);
        let restored_preview = store.trajectory_index_states().expect("states")[&session];
        assert_eq!(restored_preview.source_revision, recovered);
        assert!(!restored_preview.source_complete);
        assert!(matches!(
            observed_write(
                rollout_document("stale partial", older, second, true, 2),
                before_recovery
            ),
            Err(StoreError::TrajectorySourceChanged)
        ));
        write("restored branch", older, recovered, true, 2);
        let state = store.trajectory_index_states().expect("states")[&session];
        assert_eq!(state.source_revision, recovered);
        assert_eq!(state.source_updated_at, older);
        assert!(current(older, recovered));
        assert!(!current(older, second));
    }

    #[test]
    fn stale_writer_cannot_restore_a_title_after_an_equal_time_file_switch() {
        let directory = tempdir().expect("temporary directory");
        let store = Store::open(directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "restored");
        let older = DateTime::from_timestamp_millis(1000).expect("original time");
        let newer = DateTime::from_timestamp_millis(2000).expect("appended time");
        let first = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: None,
            missing_ancestor: None,
        };
        let second = SourceRevision {
            file: Some(Uuid::from_u128(2)),
            ..first
        };
        let observed_write = |doc, prior| {
            store.upsert_observed_trajectory_documents(&[(
                &session,
                doc,
                super::TrajectorySourceGuard::Observed(prior),
            )])
        };
        let prior = || store.trajectory_index_state(&session).expect("index state");
        let original = TrajectoryDocument {
            derived_title: Some("Previous title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            title_authoritative: true,
            ..rollout_document("original", older, first, true, 2)
        };
        observed_write(original, None).expect("save initial file");
        observed_write(
            rollout_document("appended", newer, first, false, 2),
            prior(),
        )
        .expect("preview the same growing file");
        assert_eq!(
            store.trajectory_titles().expect("titles")[&session],
            "Previous title"
        );
        let before_switch = prior();
        observed_write(
            TrajectoryDocument {
                derived_title: Some("Sampled follow-up"),
                ..rollout_document("active branch", newer, second, false, 2)
            },
            before_switch,
        )
        .expect("select the other file");
        let retired = TrajectoryDocument {
            derived_title: Some("Previous title"),
            title_authoritative: true,
            ..rollout_document("stale retired branch", newer, first, true, 2)
        };
        assert!(matches!(
            observed_write(retired, before_switch),
            Err(StoreError::TrajectorySourceChanged)
        ));
        assert!(
            !store
                .trajectory_titles()
                .expect("titles")
                .contains_key(&session)
        );
        assert!(
            store
                .search_session_trajectories("stale", 10)
                .expect("search")
                .is_empty()
        );
    }

    #[test]
    fn same_file_contents_change_replaces_the_document_and_fences_late_failures() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "in-place-restore");
        let time = DateTime::from_timestamp_millis(42).unwrap();
        let first = SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let next = SourceRevision {
            contents: Some(Uuid::from_u128(3)),
            ..first
        };
        let write = |document, prior| {
            store.upsert_observed_trajectory_documents(&[(
                &session,
                document,
                super::TrajectorySourceGuard::Observed(prior),
            )])
        };
        let original = TrajectoryDocument {
            derived_title: Some("Original title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            withdrawn_title: None,
            title_authoritative: true,
            ..rollout_document("retiredmarker", time, first, true, 2)
        };
        write(original, None).unwrap();
        let old_state = store.trajectory_index_state(&session).unwrap();
        assert!(
            !store
                .session_trajectory_source_revision_is_current(&session, time, next)
                .unwrap()
        );
        write(
            rollout_document("restoredmarker", time, next, false, 2),
            old_state,
        )
        .unwrap();
        assert_eq!(
            store.trajectory_titles().unwrap()[&session],
            "Original title"
        );
        assert_eq!(
            store
                .search_session_trajectories("restoredmarker", 10)
                .unwrap(),
            vec![session.clone()]
        );
        assert!(
            store
                .search_session_trajectories("retiredmarker", 10)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            write(original, old_state),
            Err(StoreError::TrajectorySourceChanged)
        ));

        let failure = super::TrajectoryIndexFailure {
            source_time_precise: true,
            native_summary: false,
            source_updated_at: Some(time),
            source_file: first.file,
            source_contents: first.contents,
            document_version: 2,
        };
        assert!(matches!(
            store.record_observed_trajectory_index_failure(
                &session,
                &failure,
                super::TrajectorySourceGuard::Observed(old_state)
            ),
            Err(StoreError::TrajectorySourceChanged)
        ));
        let current = store.trajectory_index_state(&session).unwrap();
        let changed_failure = super::TrajectoryIndexFailure {
            source_contents: next.contents,
            ..failure
        };
        store
            .record_observed_trajectory_index_failure(
                &session,
                &changed_failure,
                super::TrajectorySourceGuard::Observed(current),
            )
            .unwrap();
        store
            .clear_observed_trajectory_index_failure(&session, &failure)
            .unwrap();
        assert_eq!(
            store.trajectory_index_failure(&session).unwrap(),
            Some(changed_failure)
        );
        store
            .clear_observed_trajectory_index_failure(&session, &changed_failure)
            .unwrap();
        assert!(store.trajectory_index_failure(&session).unwrap().is_none());
    }

    #[test]
    fn retry_keeps_the_failed_revision_until_the_replacement_is_indexed() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "recovered-rollout");
        let time = DateTime::from_timestamp_millis(42).unwrap();
        store
            .upsert_session_trajectory(&session, "retiredmarker", time, true)
            .unwrap();
        let failed = super::TrajectoryIndexFailure {
            source_time_precise: true,
            native_summary: false,
            source_updated_at: Some(time),
            source_file: Some(Uuid::from_u128(1)),
            source_contents: Some(Uuid::from_u128(2)),
            document_version: 5,
        };
        store
            .record_trajectory_index_failure(&session, &failed)
            .unwrap();
        let obsolete = super::TrajectoryIndexFailure {
            source_contents: Some(Uuid::from_u128(3)),
            ..failed
        };
        store
            .retry_observed_trajectory_index_failure(&session, &obsolete)
            .unwrap();
        assert_eq!(
            store.trajectory_index_failure(&session).unwrap(),
            Some(failed)
        );
        store
            .retry_observed_trajectory_index_failure(&session, &failed)
            .unwrap();
        assert_eq!(
            store.trajectory_index_failure(&session).unwrap(),
            Some(super::TrajectoryIndexFailure {
                document_version: 0,
                ..failed
            })
        );
        assert!(
            store
                .search_session_trajectories("retiredmarker", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn legacy_codex_digest_requires_a_full_unstamped_v4_native_document_at_the_same_time() {
        use sha2::{Digest, Sha256};

        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "old-rollout");
        let time = DateTime::from_timestamp_millis(42).unwrap();
        let text = "synthetic legacy projection";
        let document = rollout_document(text, time, SourceRevision::default(), true, 4);
        store
            .upsert_trajectory_document(&session, &document)
            .unwrap();
        let hash =
            |session: &SessionRef, time| store.legacy_codex_projection_hash(session, time).unwrap();
        assert_eq!(
            hash(&session, time),
            Some(Sha256::digest(text.as_bytes()).into())
        );
        assert_eq!(
            hash(&session, time + chrono::Duration::milliseconds(1)),
            None
        );
        assert_eq!(
            hash(&SessionRef::new(Provider::Claude, &session.id), time),
            None
        );
        for change in [
            "origin = 'imported_bundle'",
            "source_complete = 0",
            "document_version = 3",
            "document_version = 5",
            "source_file_id = zeroblob(16)",
            "source_contents_id = zeroblob(16)",
            "missing_ancestor_id = zeroblob(16)",
            "content_hash = zeroblob(0)",
        ] {
            let connection = store.connection.borrow();
            connection.execute_batch("SAVEPOINT original").unwrap();
            connection
                .execute(&format!("UPDATE session_trajectories SET {change}"), [])
                .unwrap();
            assert_eq!(hash(&session, time), None, "{change}");
            connection
                .execute_batch("ROLLBACK TO original; RELEASE original")
                .unwrap();
        }
    }

    #[test]
    fn source_revision_migration_preserves_existing_documents_and_failures() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).expect("store");
        let session = SessionRef::new(Provider::Codex, "preserved");
        let time = DateTime::from_timestamp_millis(2000).expect("source time");
        store
            .upsert_session_trajectory(&session, "searchable marker", time, true)
            .expect("document");
        let mut failure = super::TrajectoryIndexFailure {
            source_time_precise: true,
            native_summary: false,
            source_updated_at: Some(time),
            source_file: None,
            source_contents: None,
            document_version: 4,
        };
        store
            .record_trajectory_index_failure(&session, &failure)
            .expect("failure");
        drop(store);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "DROP TRIGGER detach_changed_legacy_locator; DROP TRIGGER detach_removed_legacy_locator;
                 DROP TRIGGER clear_replaced_primary_legacy_text;
                 ALTER TABLE session_trajectories DROP COLUMN source_file_id;
                 ALTER TABLE session_trajectories DROP COLUMN missing_ancestor_id;
                 ALTER TABLE session_trajectories DROP COLUMN source_contents_id;
                 ALTER TABLE trajectory_index_failures DROP COLUMN source_file_id;
                 ALTER TABLE trajectory_index_failures DROP COLUMN source_contents_id;",
            )
            .expect("restore pre-revision schema");

        let store = Store::open(&path).expect("migrated store");
        assert_eq!(
            store.trajectory_index_states().expect("states")[&session].source_revision,
            SourceRevision::default()
        );
        assert_eq!(
            store
                .search_session_trajectories("searchable", 10)
                .expect("search"),
            vec![session.clone()]
        );
        assert_eq!(
            store.trajectory_index_failures().expect("failures")[&session],
            failure
        );
        for segment in [1, 2] {
            failure.source_file = Some(Uuid::from_u128(segment));
            store
                .record_trajectory_index_failure(&session, &failure)
                .expect("replaced failure");
            assert_eq!(
                store.trajectory_index_failures().expect("failures")[&session],
                failure
            );
        }
    }

    #[test]
    fn full_read_with_provider_omissions_upgrades_same_state_document() {
        let temporary_directory = tempdir().expect("temporary directory");
        let store = Store::open(temporary_directory.path().join("store.sqlite3")).expect("store");
        let session = SessionRef::new(Provider::Codex, "omissions");
        let source_updated_at = Utc::now();
        let text = "retained tool history";
        let document = |source_complete| TrajectoryDocument {
            redacted_text: text,
            source_revision: SourceRevision::default(),
            source_updated_at,
            source_byte_count: text.len() + 1,
            indexed_byte_count: text.len(),
            truncation_strategy: "source_incomplete_event_head_tail",
            source_complete,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 2,
            derived_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
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
            redacted_text: text,
            source_revision: SourceRevision::default(),
            source_updated_at,
            source_byte_count: str::len(text),
            indexed_byte_count: str::len(text),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
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
            redacted_text: native_text,
            source_revision: SourceRevision::default(),
            source_updated_at,
            source_byte_count: native_text.len() + 256,
            indexed_byte_count: native_text.len(),
            truncation_strategy: "document_head_tail",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 7,
            derived_title: Some("Synthetic native title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            title_authoritative: true,
        };
        let imported_document = TrajectoryDocument {
            redacted_text: imported_text,
            source_revision: SourceRevision::default(),
            source_updated_at,
            source_byte_count: imported_text.len(),
            indexed_byte_count: imported_text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::ImportedBundle,
            document_version: 8,
            derived_title: Some("Synthetic imported title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
            title_authoritative: true,
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
            redacted_text: text,
            source_revision: SourceRevision::default(),
            source_updated_at,
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
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
            redacted_text: &text,
            source_revision: SourceRevision::default(),
            source_updated_at: Utc::now(),
            source_byte_count: text.len(),
            indexed_byte_count: text.len(),
            truncation_strategy: "none",
            source_complete: true,
            provider_limited: false,
            origin: SessionTrajectoryOrigin::Native,
            document_version: 1,
            derived_title: None,
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            database_step_proof: None,
            desktop_projection: None,
            legacy_utf8_text: None,
            withdrawn_title: None,
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
    fn cached_sessions_follow_observed_homes_and_keep_unqualified_history() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("cache.sqlite3")).unwrap();
        let provider = Provider::Antigravity;
        let a = directory.path().join("a");
        let b = directory.path().join("b");
        let mut seen = indexed_session(provider, "seen", "Current title");
        let legacy = indexed_session(provider, "legacy", "Offline title");
        store.upsert_indexed_session(&legacy).unwrap();
        let visible = |home: &std::path::Path, session: &SessionRef| {
            store
                .indexed_sessions_in_homes(Some(home), None)
                .unwrap()
                .iter()
                .any(|row| &row.session == session)
        };
        let fresh_since = Utc::now() - chrono::Duration::seconds(15);
        store
            .mark_session_discovery_attempted(provider, 7, &a)
            .unwrap();
        for (home, other) in [(&a, &b), (&b, &a)] {
            store
                .cache_discovered_sessions(
                    provider,
                    std::slice::from_ref(&seen),
                    super::DiscoveryScope::PartialProvider,
                    Some(home),
                )
                .unwrap();
            assert!(visible(home, &seen.session));
            assert!(!visible(other, &seen.session));
            assert!(visible(other, &legacy.session));
        }
        assert!(matches!(
            store
                .try_begin_session_discovery(provider, 7, &a, fresh_since)
                .unwrap(),
            SessionDiscoveryClaim::Claimed(_)
        ));
        assert_eq!(
            store.indexed_sessions_in_homes(None, None).unwrap(),
            vec![legacy.clone()]
        );
        assert_eq!(
            store
                .indexed_sessions_for_provider_in_home(provider, &a)
                .unwrap(),
            vec![legacy]
        );
        seen.title = Some("Unqualified writer changed title".to_owned());
        store.upsert_indexed_session(&seen).unwrap();
        assert!(visible(&a, &seen.session));
        assert!(visible(&b, &seen.session));
    }

    #[test]
    fn confirmed_ide_archives_remove_only_catalog_rows_in_the_observed_home() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let provider = Provider::AntigravityIde;
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let archive_ids = ["archived".to_owned(), "other-home".to_owned()];
        let archive = |home: &Path| {
            let attempt = store
                .mark_session_discovery_attempted(provider, 15, home)
                .unwrap();
            store
                .publish_session_discovery(
                    &attempt,
                    &[],
                    super::DiscoveryScope::PartialProvider
                        .with_source_files(&|_| None)
                        .with_archived_desktop_ids(&archive_ids),
                    Utc::now(),
                )
                .unwrap();
        };
        for (home, id) in [(&a, "missing"), (&a, "archived"), (&b, "other-home")] {
            store
                .cache_discovered_sessions(
                    provider,
                    &[indexed_session(provider, id, id)],
                    super::DiscoveryScope::PartialProvider,
                    Some(home),
                )
                .unwrap();
        }
        archive(&a);
        let visible = |home: &Path| {
            store
                .indexed_sessions_for_provider_in_home(provider, home)
                .unwrap()
                .into_iter()
                .map(|row| row.session.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(&a), ["missing"]);
        assert_eq!(visible(&b), ["other-home"]);
    }

    #[test]
    fn ide_listings_keep_only_explicit_database_counts_in_the_selected_home() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let store = Store::open(home.join("store.sqlite3")).unwrap();
        let file = Uuid::from_u128(1);
        let row = IndexedSession {
            event_count: 2,
            source_revision: Some(SourceRevision {
                file: Some(file),
                ..Default::default()
            }),
            ..indexed_session(Provider::AntigravityIde, "one", "IDE")
        };
        let listing = super::DiscoveryScope::Workspace;
        let publish = |count| {
            store
                .cache_discovered_sessions(
                    row.session.provider,
                    std::slice::from_ref(&row),
                    listing
                        .clone()
                        .with_source_files(&|_| Some(file))
                        .with_database_alias_counts(&|_| count),
                    Some(home),
                )
                .unwrap();
        };
        let read = |provider, home: &Path, file| {
            store
                .verified_desktop_step_count(&SessionRef::new(provider, "one"), home, file)
                .unwrap()
        };
        publish(None);
        assert_eq!(read(row.session.provider, home, file), None);
        publish(Some(2));
        publish(None);
        assert_eq!(read(row.session.provider, home, file), Some(2));
        assert_eq!(read(Provider::AntigravityDesktop, home, file), Some(2));
        assert_eq!(read(row.session.provider, &home.join("other"), file), None);
        assert_eq!(read(row.session.provider, home, Uuid::from_u128(2)), None);
        publish(Some(0));
        assert_eq!(read(row.session.provider, home, file), Some(0));
        store.forget_session(&row.session).unwrap();
        assert_eq!(read(row.session.provider, home, file), None);
    }

    #[test]
    fn cached_desktop_summary_tracks_the_selected_database() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let store = Store::open(home.join("store.sqlite3")).unwrap();
        let provider = Provider::AntigravityDesktop;
        let revision = |file, contents| {
            Some(SourceRevision {
                file: Some(Uuid::from_u128(file)),
                contents: Some(Uuid::from_u128(contents)),
                ..SourceRevision::default()
            })
        };
        let named = IndexedSession {
            event_count: 3,
            updated_at_approximate: true,
            source_revision: revision(1, 1),
            ..indexed_session(provider, "session", "Desktop title")
        };
        let record = |row: &IndexedSession, scope| {
            store
                .cache_discovered_sessions(provider, std::slice::from_ref(row), scope, Some(home))
                .unwrap();
        };
        let verified_count = |home: &Path, file| {
            store
                .verified_desktop_step_count(&named.session, home, Uuid::from_u128(file))
                .unwrap()
        };
        record(&named, super::DiscoveryScope::PartialProvider);
        assert_eq!(verified_count(&home.join("other"), 1), None);
        assert_eq!(verified_count(home, 3), None);
        for (current, count) in [
            (revision(1, 2), 3),
            (revision(2, 2), 0),
            (revision(2, 3), 0),
            (None, 0),
        ] {
            let observed = IndexedSession {
                source_revision: current,
                title: None,
                project_path: None,
                git_branch: None,
                event_count: 0,
                ..named.clone()
            };
            record(
                &observed,
                super::DiscoveryScope::DesktopDatabasesWithoutSummaries,
            );
            let (rows, _) = store
                .cached_picker_sessions_with_titles_in_homes(None, Some(home))
                .unwrap();
            assert_eq!(
                rows,
                [IndexedSession {
                    source_revision: current,
                    event_count: count,
                    ..named.clone()
                }]
            );
            assert_eq!(verified_count(home, 1), Some(3));
            assert_eq!(verified_count(home, 2), None);
        }
    }

    #[test]
    fn desktop_discovery_snapshots_keep_only_returned_ids() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let provider = Provider::AntigravityDesktop;
        let home = directory.path().join("active");
        let other = directory.path().join("other");
        let current = indexed_session(provider, "current", "Current");
        let archived = indexed_session(provider, "archived", "Archived");
        let claim = |home: &Path| {
            store
                .mark_session_discovery_attempted(provider, 7, home)
                .unwrap()
        };
        let publish = |owner: &super::SessionDiscoveryAttempt, rows: &[IndexedSession]| {
            store
                .publish_session_discovery(
                    owner,
                    rows,
                    super::DiscoveryScope::PartialProvider,
                    Utc::now(),
                )
                .unwrap()
        };
        let snapshot = |home: &Path| store.published_desktop_sessions(7, home).unwrap();
        let membership = |home: &Path| store.published_desktop_session_ids(7, home).unwrap();
        assert_eq!(snapshot(&home), None);
        assert_eq!(membership(&other), None);

        let mut listed = [current.clone(), archived.clone()];
        assert!(publish(&claim(&home), &listed));
        store.connection.borrow().execute_batch(
            "CREATE TEMP TRIGGER reject_membership_rewrite BEFORE UPDATE ON desktop_discovery_snapshots
             BEGIN SELECT RAISE(ABORT, 'unexpected snapshot rewrite'); END",
        ).unwrap();
        listed.reverse();
        assert!(publish(&claim(&home), &listed));
        store
            .connection
            .borrow()
            .execute_batch("DROP TRIGGER reject_membership_rewrite")
            .unwrap();

        assert!(publish(&claim(&home), std::slice::from_ref(&current)));
        assert_eq!(
            membership(&home),
            Some(HashSet::from([current.session.id.clone()]))
        );
        assert_eq!(snapshot(&home), Some(vec![current.clone()]));
        assert_eq!(
            store
                .indexed_sessions_for_provider_in_home(provider, &home)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.published_desktop_sessions(8, &home).unwrap(), None);

        let superseded = claim(&home);
        let failed = claim(&home);
        assert!(!publish(&superseded, &[]));
        assert!(store.finish_session_discovery(&failed, Utc::now()).unwrap());
        assert_eq!(snapshot(&home), Some(vec![current.clone()]));
        assert!(publish(&claim(&other), std::slice::from_ref(&current)));
        assert_eq!(snapshot(&home), Some(Vec::new()));
        assert_eq!(snapshot(&other), Some(vec![current]));
        assert!(publish(&claim(&other), &[]));
        assert_eq!(snapshot(&other), Some(Vec::new()));
        assert_eq!(membership(&other), Some(HashSet::new()));
    }

    #[test]
    fn discovery_subscribers_see_finished_results_after_the_lease_expires() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let home = directory.path();
        let provider = Provider::Antigravity;
        let owner = store
            .mark_session_discovery_attempted(provider, 7, home)
            .unwrap();
        let earlier = Utc::now() - chrono::Duration::seconds(20);
        let cutoff = Utc::now() - chrono::Duration::seconds(15);
        assert!(
            store
                .publish_session_discovery(
                    &owner,
                    &[],
                    super::DiscoveryScope::PartialProvider,
                    earlier
                )
                .unwrap()
        );
        assert_eq!(
            store.observe_session_discovery(&owner, cutoff).unwrap(),
            SessionDiscoveryState::Published
        );
        let SessionDiscoveryClaim::Claimed(next) = store
            .try_begin_session_discovery(provider, 7, home, cutoff)
            .unwrap()
        else {
            panic!("expected a new check after the published result expired");
        };
        assert!(store.finish_session_discovery(&next, earlier).unwrap());
        assert_eq!(
            store.observe_session_discovery(&next, cutoff).unwrap(),
            SessionDiscoveryState::Failed
        );
    }

    #[test]
    fn discovery_waiters_see_only_committed_provider_results() {
        use super::DiscoveryScope::{PartialProvider, Workspace};

        let directory = tempdir().unwrap();
        let db = directory.path().join("cache.sqlite3");
        let home = directory.path();
        let provider = Provider::Antigravity;
        let since = Utc::now() - chrono::Duration::seconds(15);
        let prior = Store::open(&db).unwrap();
        let first = prior
            .mark_session_discovery_attempted(provider, 7, home)
            .unwrap();
        prior
            .connection
            .borrow()
            .execute_batch("DROP TABLE session_index_discovery_results")
            .unwrap();
        drop(prior);
        let writer = Store::open(&db).unwrap();
        let reader = Store::open(&db).unwrap();
        let claim = || {
            reader
                .try_begin_session_discovery(provider, 7, home, since)
                .unwrap()
        };
        let state = || reader.observe_session_discovery(&first, since).unwrap();
        let catalog = || {
            reader
                .indexed_sessions_for_provider_in_home(provider, home)
                .unwrap()
        };
        let row = indexed_session(provider, "restored", "Restored native session");
        let rows = std::slice::from_ref(&row);

        assert_eq!(claim(), SessionDiscoveryClaim::InProgress(first.clone()));
        assert!(
            !writer
                .publish_session_discovery(&first, rows, Workspace, Utc::now())
                .unwrap()
        );
        assert!(catalog().is_empty());
        assert_eq!(state(), SessionDiscoveryState::InProgress);

        writer
            .connection
            .borrow()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_catalog_write BEFORE INSERT ON session_index
             BEGIN SELECT RAISE(ABORT, 'fixture'); END",
            )
            .unwrap();
        assert!(
            writer
                .publish_session_discovery(&first, rows, PartialProvider, Utc::now())
                .is_err()
        );
        assert_eq!(state(), SessionDiscoveryState::InProgress);
        assert!(catalog().is_empty());
        writer
            .connection
            .borrow()
            .execute_batch("DROP TRIGGER refuse_catalog_write")
            .unwrap();

        assert!(
            writer
                .publish_session_discovery(&first, rows, PartialProvider, Utc::now())
                .unwrap()
        );
        assert_eq!(state(), SessionDiscoveryState::Published);
        assert_eq!(claim(), SessionDiscoveryClaim::Fresh);
        assert_eq!(catalog(), rows);
        assert!(!writer.finish_session_discovery(&first, Utc::now()).unwrap());

        // Older readers replace the lease without writing a result for its new ID.
        let next = Uuid::new_v4();
        writer
            .connection
            .borrow()
            .execute(
                "UPDATE session_index_reader_checks SET attempt_id = ?1",
                [next.into_bytes()],
            )
            .unwrap();
        assert_eq!(state(), SessionDiscoveryState::Superseded);
        let SessionDiscoveryClaim::InProgress(pending) = claim() else {
            panic!("expected the old reader's pending discovery");
        };
        assert_eq!(pending.id, next);
        assert!(
            writer
                .finish_session_discovery(&pending, Utc::now())
                .unwrap()
        );
        assert_eq!(
            reader.observe_session_discovery(&pending, since).unwrap(),
            SessionDiscoveryState::Failed
        );
        assert_eq!(claim(), SessionDiscoveryClaim::Fresh);
        assert_eq!(catalog(), rows);
    }

    #[test]
    fn discovery_attempts_expire_per_reader_and_selected_home() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("cache.sqlite3");
        let store = Store::open(&database).unwrap();
        let provider = Provider::Antigravity;
        let home = directory.path().join("one");
        let other = directory.path().join("two");
        let fresh_since = Utc::now() - chrono::Duration::seconds(15);
        let claim = |version, home: &std::path::Path| {
            matches!(
                store
                    .try_begin_session_discovery(provider, version, home, fresh_since)
                    .unwrap(),
                SessionDiscoveryClaim::Claimed(_)
            )
        };
        store.mark_session_index_checked(provider).unwrap();
        store.merge_workspace_sessions(provider, &[]).unwrap();
        assert!(claim(7, &home));
        assert!(!claim(7, &home));
        assert!(claim(8, &home));
        assert!(!claim(7, &home));
        let obsolete = store
            .mark_session_discovery_attempted(provider, 7, &home)
            .unwrap();
        assert!(claim(7, &other));
        assert!(claim(7, &home));

        store
            .connection
            .borrow()
            .execute(
                "UPDATE session_index_reader_checks SET checked_at = 1 WHERE reader_version = 7",
                [],
            )
            .unwrap();
        store.mark_session_index_checked(provider).unwrap();
        store.merge_workspace_sessions(provider, &[]).unwrap();
        assert!(
            !store
                .finish_session_discovery(&obsolete, Utc::now())
                .unwrap()
        );
        assert_eq!(
            store
                .observe_session_discovery(&obsolete, fresh_since)
                .unwrap(),
            SessionDiscoveryState::Superseded
        );
        assert!(
            !store
                .publish_session_discovery(
                    &obsolete,
                    &[indexed_session(provider, "stale", "Stale")],
                    super::DiscoveryScope::PartialProvider,
                    Utc::now()
                )
                .unwrap()
        );
        assert!(
            store
                .indexed_sessions_for_provider(provider)
                .unwrap()
                .is_empty()
        );
        let writers = [
            Store::open(&database).unwrap(),
            Store::open(&database).unwrap(),
        ];
        let barrier = std::sync::Barrier::new(writers.len());
        let results = std::thread::scope(|scope| {
            let barrier = &barrier;
            let home = &home;
            let waits = writers.map(|writer| {
                scope.spawn(move || {
                    barrier.wait();
                    matches!(
                        writer
                            .try_begin_session_discovery(provider, 7, home, fresh_since)
                            .unwrap(),
                        SessionDiscoveryClaim::Claimed(_)
                    )
                })
            });
            waits.map(|wait| wait.join().unwrap())
        });
        assert_ne!(results[0], results[1]);
    }

    #[test]
    fn failed_discovery_can_finish_after_lease_expiry_and_waiters_read_under_a_writer_lock() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("cache.sqlite3");
        let store = Store::open(&database).unwrap();
        let provider = Provider::AntigravityDesktop;
        let home = directory.path();
        let fresh_since = Utc::now() - chrono::Duration::seconds(15);
        let current = store
            .mark_session_discovery_attempted(provider, 7, home)
            .unwrap();
        store
            .connection
            .borrow()
            .execute(
                "UPDATE session_index_reader_checks SET checked_at = 1 WHERE reader_version = 7",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .observe_session_discovery(&current, fresh_since)
                .unwrap(),
            SessionDiscoveryState::Expired
        );
        assert!(
            store
                .finish_session_discovery(&current, Utc::now())
                .unwrap()
        );
        assert_eq!(
            store
                .observe_session_discovery(&current, fresh_since)
                .unwrap(),
            SessionDiscoveryState::Failed
        );

        let mut writer = rusqlite::Connection::open(&database).unwrap();
        let _active = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        store
            .connection
            .borrow()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        assert_eq!(
            store
                .try_begin_session_discovery(provider, 7, home, fresh_since)
                .unwrap(),
            SessionDiscoveryClaim::Fresh
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
    fn existing_session_index_preserves_rows_with_unknown_timestamp_accuracy_and_source() {
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
                ) WITHOUT ROWID;
                INSERT INTO session_index(provider, session_id, title, event_count, indexed_at)
                    VALUES ('codex', 'old', 'Legacy title', 1, 0);",
            )
            .expect("old schema");
        drop(connection);

        let store = Store::open(&database).expect("upgraded store");
        let session_ref = SessionRef::new(Provider::Codex, "old");
        assert!(store.indexed_session(&session_ref).unwrap().is_none());
        let legacy = store
            .observe_codex_catalog_inventory()
            .unwrap()
            .observed
            .pop()
            .unwrap();
        assert_eq!(legacy.title.as_deref(), Some("Legacy title"));
        assert_eq!(legacy.source_revision, None);
        assert!(!legacy.updated_at_approximate);
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
    fn legacy_writer_after_upgrade_keeps_its_text_and_bundle_protection() {
        let directory = tempdir().expect("temporary directory");
        let database = directory.path().join("store.sqlite3");
        let store = Store::open(&database).expect("store");
        let write_native = |id| {
            store
                .upsert_session_trajectory(
                    &SessionRef::new(Provider::Codex, id),
                    "retained",
                    Utc::now(),
                    true,
                )
                .expect("write native trajectory");
        };
        write_native("before-insert");
        write_native("before-update");

        // Older writers save the bundle without setting protection on the source trajectory.
        for id in ["before-insert", "before-update", "after-bundle"] {
            store.connection.borrow().execute(
                "INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES ('fixture', ?1, 1)
                 ON CONFLICT (bundle_id) DO UPDATE SET bundle_json = excluded.bundle_json",
                [json!({"snapshot": {"session": {"provider": "codex", "id": id}}}).to_string()],
            ).expect("save bundle with older writer");
        }
        write_native("after-bundle");
        store
            .replace_indexed_sessions(Provider::Codex, &[])
            .expect("prune unobserved sessions");
        assert_eq!(
            store
                .search_session_trajectories("retained", 10)
                .expect("protected sources")
                .len(),
            3
        );

        store.connection.borrow().execute(
            "INSERT INTO session_trajectories (
                 provider, session_id, redacted_text, content_hash, source_updated_at, source_complete,
                 complete, indexed_at, source_byte_count, indexed_byte_count, truncation_strategy,
                 origin, protected_by_bundle
             ) VALUES ('codex', 'legacy-parent', 'legacyparentmarker', X'', 1, 0, 0, 1, 18, 18,
                       'legacy_unknown', 'native', 0)",
            [],
        ).expect("write legacy parent text");
        drop(store);
        let reopened = Store::open(database).expect("read legacy writer data");
        assert_eq!(
            reopened
                .search_session_trajectories("legacyparentmarker", 10)
                .expect("chunked text"),
            [SessionRef::new(Provider::Codex, "legacy-parent")]
        );
    }

    #[test]
    fn codex_discovery_freshness_belongs_to_the_scanned_home() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        let use_home = |store: &Store, home: Option<&str>| {
            store
                .connection
                .borrow()
                .execute("UPDATE current_codex_home SET home=?1", [home])
                .unwrap();
        };
        let freshness = |store: &Store| {
            (
                store.session_index_checked_at(Provider::Codex).unwrap(),
                store.session_index_refreshed_at(Provider::Codex).unwrap(),
            )
        };
        first
            .connection
            .borrow()
            .execute_batch(
                "INSERT INTO session_index_state VALUES ('codex', 1000);
             INSERT INTO session_index_checks VALUES ('codex', 1000);",
            )
            .unwrap();

        use_home(&first, Some("/synthetic/first"));
        use_home(&second, Some("/synthetic/second"));
        assert_eq!(freshness(&first), (None, None));
        first
            .replace_indexed_sessions(Provider::Codex, &[])
            .unwrap();
        let first_scan = freshness(&first);
        assert!(first_scan.0.is_some() && first_scan.1.is_some());
        assert_eq!(freshness(&second), (None, None));
        second.mark_session_index_checked(Provider::Codex).unwrap();
        let second_check = freshness(&second);
        assert!(second_check.0.is_some() && second_check.1.is_none());
        assert_eq!(freshness(&first), first_scan);

        use_home(&second, None);
        second
            .replace_indexed_sessions(Provider::Codex, &[])
            .unwrap();
        second.mark_session_index_checked(Provider::Codex).unwrap();
        assert_eq!(freshness(&second), (None, None));
        use_home(&second, Some("/synthetic/second"));
        assert_eq!(freshness(&second), second_check);
    }

    #[test]
    fn losing_a_codex_catalog_row_expires_its_previous_home() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        for (store, home) in [(&first, "/synthetic/first"), (&second, "/synthetic/second")] {
            store
                .connection
                .borrow()
                .execute("UPDATE current_codex_home SET home=?1", [home])
                .unwrap();
        }
        let first_row = indexed_session(Provider::Codex, "shared", "First home");
        let second_row = indexed_session(Provider::Codex, "shared", "Second home");
        let check = |store: &Store| store.session_index_checked_at(Provider::Codex).unwrap();
        let refresh = |store: &Store| store.session_index_refreshed_at(Provider::Codex).unwrap();
        first
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&first_row))
            .unwrap();
        let first_scan = (check(&first), refresh(&first));
        assert!(first_scan.0.is_some() && first_scan.1.is_some());
        first.upsert_indexed_session(&first_row).unwrap();
        assert_eq!((check(&first), refresh(&first)), first_scan);

        second.mark_session_index_checked(Provider::Codex).unwrap();
        let second_check = check(&second);
        second.upsert_indexed_session(&second_row).unwrap();
        assert_eq!((check(&first), refresh(&first)), (None, None));
        assert!(second_check.is_some());
        assert_eq!(check(&second), second_check);

        second
            .remove_unavailable_codex_sessions(&[second_row])
            .unwrap();
        assert_eq!(check(&second), None);
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
        for id in ["replaceable", "insert-only", "unmatched"] {
            store
                .upsert_indexed_session(&indexed_session(Provider::Codex, id, id))
                .expect("record the home of the session");
        }
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
