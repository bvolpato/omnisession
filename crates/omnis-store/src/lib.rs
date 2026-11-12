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

mod codex_document_home;
mod database_receipts;
mod discovery;
use discovery::{
    initialize_discovery_checks, initialize_session_index_checks, record_discovery_home,
};
pub mod search_query;

const DATABASE_FILE_NAME: &str = "store.sqlite3";

/// Format of the store, kept in `PRAGMA user_version`.
///
/// Format 1 stores chunk text compressed. A version that finds a higher number does not open the
/// store, because it cannot tell what a later version changed. Versions before format 1 do not
/// read the number. They fail when they open a format 1 store, because the chunk table is a view.
/// Format 2 keeps separate search documents for Codex homes that contain the same session ID.
const STORE_FORMAT: i64 = 2;

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

const VISIBLE_CATALOG_ROW: &str = "(catalog.provider NOT IN ('pi','omp') OR NOT EXISTS (
                    SELECT 1 FROM scoped_trajectory_failures AS failed
                    WHERE failed.provider = catalog.provider AND failed.session_id = catalog.session_id
                      AND failed.source_file_id IS NOT NULL AND failed.source_contents_id IS NOT NULL
                      AND (failed.source_file_id IS NOT catalog.source_file_id
                           OR failed.source_contents_id IS NOT catalog.source_contents_id)))";

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
         INNER JOIN visible_trajectories AS trajectories
             ON trajectories.id = chunks.trajectory_id
         {SEARCH_GAP_JOIN}
         LEFT JOIN antigravity_gap_projections all_projections ON all_projections.trajectory_id=trajectories.id
         INNER JOIN eligible
             ON eligible.provider = trajectories.provider
            AND eligible.session_id = trajectories.session_id
            AND (eligible.file IS NULL OR (eligible.file = hex(trajectories.source_file_id)
                 AND eligible.contents = hex({SEARCH_ACTIVE_CONTENTS})))
            AND (all_projections.trajectory_id IS NULL OR
                 (chunks.chunk_index>=all_projections.first_chunk)=(active_projection.trajectory_id IS NOT NULL))
            AND (eligible.file IS NOT NULL OR NOT EXISTS (
                 SELECT 1 FROM observed_session_index AS catalog WHERE ({CONFLICTING_FILE_CATALOG}) OR ({CONFLICTING_CODEX_CATALOG})))
            AND NOT EXISTS (SELECT 1 FROM scoped_trajectory_failures AS failed
                            WHERE ({HIDDEN_FAILED_TRAJECTORY}) OR ({CONFLICTING_CODEX_FAILURE}))"
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
        source_file_id, missing_ancestor_id, source_contents_id, projected_byte_count, projected_record_prefix_hash, sampled_file_bytes, fully_scanned_bytes, verified_record_time
    ) VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?16, ?17, ?18, ?20, ?21, ?22, ?25, ?26, ?27, ?28, ?29)
    ON CONFLICT DO UPDATE SET
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
        projected_byte_count = excluded.projected_byte_count,
        sampled_file_bytes = excluded.sampled_file_bytes,
        fully_scanned_bytes = excluded.fully_scanned_bytes,
        verified_record_time = excluded.verified_record_time, recovered_anchor_bytes = NULL,
        recovered_gap_start = NULL, observed_text_bytes = NULL, observed_text_hash = NULL,
        projected_prefix_hash = NULL,
        projected_record_prefix_hash = excluded.projected_record_prefix_hash,
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
            -- Older Antigravity readers copied the catalog name into the conversation title.
            WHEN excluded.provider IN ('antigravity', 'antigravity-desktop')
                AND excluded.origin = 'native' AND excluded.source_file_id IS NOT NULL
                AND excluded.document_version > session_trajectories.document_version
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
      AND (excluded.provider NOT IN ('antigravity', 'antigravity-desktop')
           OR excluded.origin <> 'native' OR session_trajectories.origin <> 'native'
           OR excluded.source_complete OR excluded.provider_limited
           OR ?24
           OR (NOT session_trajectories.provider_limited AND
               (NOT session_trajectories.source_complete
                OR (excluded.source_file_id IS NULL AND session_trajectories.source_file_id IS NULL))))
      AND (excluded.provider NOT IN ('pi','omp','codex') OR excluded.origin <> 'native'
           OR session_trajectories.origin <> 'native' OR (
               (excluded.source_file_id IS NOT NULL OR session_trajectories.source_file_id IS NULL)
               AND (excluded.source_file_id IS NULL OR session_trajectories.source_file_id IS NOT NULL
                    OR NOT session_trajectories.source_complete
                    OR ((excluded.source_complete OR excluded.provider_limited) AND
                       (?23 OR NOT EXISTS (SELECT 1 FROM scoped_session_index AS catalog
                            WHERE catalog.provider = excluded.provider AND catalog.session_id = excluded.session_id
                              AND catalog.source_file_id IS NULL))))))
      AND (
          ?24 OR (excluded.source_complete, excluded.provider_limited) >=
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
                               OR (session_trajectories.tail_reader_version = 0 AND ?30)
                               OR excluded.source_byte_count <>
                                   session_trajectories.source_byte_count
                               OR excluded.indexed_byte_count <>
                                   session_trajectories.indexed_byte_count
                               OR (excluded.projected_byte_count IS NOT NULL AND
                                   (excluded.verified_record_time IS NOT session_trajectories.verified_record_time OR excluded.fully_scanned_bytes IS NOT session_trajectories.fully_scanned_bytes OR excluded.sampled_file_bytes IS NOT session_trajectories.sampled_file_bytes OR excluded.projected_byte_count IS NOT session_trajectories.projected_byte_count OR excluded.projected_record_prefix_hash IS NOT session_trajectories.projected_record_prefix_hash OR session_trajectories.projected_prefix_hash IS NOT NULL))
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
const UPSERT_CODEX_PARENT_SQL: &str = "
    INSERT INTO session_trajectories (
        provider, session_id, redacted_text, content_hash,
        source_updated_at, source_complete,
        complete, indexed_at, source_byte_count, indexed_byte_count,
        truncation_strategy, origin, protected_by_bundle, document_version, derived_title,
        source_file_id, missing_ancestor_id, source_contents_id, derived_title_is_verified, derived_title_is_local_opener, source_updated_submillis, provider_limited, title_authoritative, codex_home
    ) VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?18, ?20,
              CASE WHEN ?20 THEN ?22 END, 0, ?24, ?25, CASE WHEN ?11='native' THEN (SELECT home FROM current_codex_home) END)
    ON CONFLICT DO UPDATE SET
        content_hash = excluded.content_hash,
        source_updated_at = excluded.source_updated_at,
        source_file_id = excluded.source_file_id,
        source_contents_id = excluded.source_contents_id,
        missing_ancestor_id = excluded.missing_ancestor_id,
        source_complete = excluded.source_complete,
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
        codex_workspace_current = 1,
        source_updated_submillis=excluded.source_updated_submillis, provider_limited=excluded.provider_limited, title_authoritative=excluded.title_authoritative,
        (derived_title, derived_title_is_verified, derived_title_is_local_opener) = (
            SELECT CASE WHEN replace_title THEN excluded.derived_title
                        ELSE session_trajectories.derived_title END,
                   CASE WHEN replace_title THEN excluded.derived_title_is_verified
                            OR (NOT upgrading_title AND excluded.derived_title IS NOT NULL AND session_trajectories.derived_title_is_verified
                                AND excluded.source_file_id IS session_trajectories.source_file_id
                                AND excluded.derived_title IS session_trajectories.derived_title)
                        ELSE session_trajectories.derived_title_is_verified END,
                   CASE WHEN NOT coalesce(replace_title, 0) THEN session_trajectories.derived_title_is_local_opener
                        WHEN same_title AND NOT upgrading_title AND session_trajectories.derived_title_is_verified AND (
                             NOT excluded.derived_title_is_verified OR
                             (session_trajectories.derived_title_is_local_opener IS 0 AND ?22))
                          THEN session_trajectories.derived_title_is_local_opener
                        ELSE excluded.derived_title_is_local_opener END
            FROM (SELECT
                excluded.source_file_id IS session_trajectories.source_file_id
                AND excluded.derived_title IS NOT NULL AND excluded.derived_title IS session_trajectories.derived_title AS same_title,
                CASE WHEN upgrading_title OR NOT session_trajectories.codex_workspace_current THEN 1
                     WHEN ?23 THEN session_trajectories.origin = 'native'
                          AND excluded.source_file_id = session_trajectories.source_file_id
                          AND ?21 = session_trajectories.derived_title AND session_trajectories.derived_title_is_local_opener IS NOT 0
                     WHEN ?22 AND excluded.source_file_id = session_trajectories.source_file_id
                         AND session_trajectories.origin = 'native'
                         AND NOT (excluded.source_complete AND excluded.truncation_strategy IN (
                             'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'))
                     THEN session_trajectories.derived_title_is_local_opener IS NOT 0
                     ELSE (excluded.derived_title IS NOT NULL AND (
                    NOT ?17 OR excluded.derived_title_is_verified
                    OR NOT session_trajectories.derived_title_is_verified
                    OR excluded.source_file_id IS NOT session_trajectories.source_file_id))
                OR (?17 AND excluded.source_file_id IS NOT NULL
                        AND excluded.source_file_id IS NOT session_trajectories.source_file_id)
                OR (?17 AND excluded.source_complete AND excluded.truncation_strategy IN (
                    'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'
                ))
                OR (?21 = session_trajectories.derived_title
                    AND NOT session_trajectories.derived_title_is_verified
                    AND session_trajectories.origin = 'native'
                    AND excluded.source_file_id = session_trajectories.source_file_id)
                END AS replace_title, upgrading_title
                FROM (SELECT ?17 AND session_trajectories.origin = 'native' AND excluded.origin = 'native'
                    AND session_trajectories.document_version < 7 AND excluded.document_version >= 7
                    AND session_trajectories.derived_title_is_local_opener IS NOT 1 AS upgrading_title))
        )
    WHERE excluded.document_version >= session_trajectories.document_version AND (
       NOT session_trajectories.codex_workspace_current
       OR excluded.document_version > session_trajectories.document_version
       OR excluded.source_updated_at > session_trajectories.source_updated_at
       OR (?17 AND excluded.provider = 'codex' AND excluded.origin = 'native'
           AND session_trajectories.origin = 'native' AND excluded.source_complete
           AND excluded.source_file_id = session_trajectories.source_file_id
           AND excluded.source_contents_id IS session_trajectories.source_contents_id
           AND (excluded.derived_title_is_verified OR (excluded.source_complete AND excluded.truncation_strategy IN (
               'none', 'event_head_tail', 'document_head_tail', 'event_and_document_head_tail'
           ))) AND (excluded.derived_title IS NOT session_trajectories.derived_title
                    OR (excluded.derived_title_is_verified AND NOT session_trajectories.derived_title_is_verified)))
       OR (?19 AND excluded.source_complete AND excluded.origin = 'native'
           AND session_trajectories.origin = 'native' AND (
               excluded.content_hash <> session_trajectories.content_hash
               OR excluded.truncation_strategy <> session_trajectories.truncation_strategy))
       OR (?17 AND excluded.origin = 'native' AND session_trajectories.origin = 'native'
           AND (excluded.source_updated_at < session_trajectories.source_updated_at
                OR (excluded.source_file_id IS NOT NULL
                    AND (excluded.source_file_id IS NOT session_trajectories.source_file_id
                         OR (excluded.source_contents_id IS NOT NULL
                             AND excluded.source_contents_id IS NOT session_trajectories.source_contents_id)))))
       OR (
           excluded.source_updated_at = session_trajectories.source_updated_at
           AND (
               excluded.source_complete > session_trajectories.source_complete
               -- A restored ancestor can make the history too large for a full read.
               OR (?17 AND excluded.origin = 'native' AND session_trajectories.origin = 'native'
                   AND excluded.source_file_id IS NOT NULL
                   AND excluded.source_file_id IS session_trajectories.source_file_id
                   AND session_trajectories.missing_ancestor_id IS NOT NULL
                   AND excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id)
               OR (
                   excluded.source_complete = session_trajectories.source_complete
                   AND (
                       excluded.complete > session_trajectories.complete
                       OR (
                           excluded.complete = session_trajectories.complete
                           AND (
                               excluded.content_hash <> session_trajectories.content_hash
                               OR (session_trajectories.tail_reader_version = 0 AND ?26)
                               OR excluded.source_byte_count <>
                                   session_trajectories.source_byte_count
                               OR excluded.indexed_byte_count <>
                                   session_trajectories.indexed_byte_count
                               OR excluded.truncation_strategy <>
                                   session_trajectories.truncation_strategy
                               OR excluded.missing_ancestor_id IS NOT session_trajectories.missing_ancestor_id
                               OR excluded.origin <> session_trajectories.origin
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

/// Effective workspace from a read of a specific Codex rollout revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedCodexWorkspace {
    pub source_path: PathBuf,
    pub source_stamp: String,
    pub project_path: Option<PathBuf>,
    pub git_branch: Option<String>,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "source coverage and provider title evidence are independent properties"
)]
pub struct TrajectoryDocument<'a> {
    pub source_revision: SourceRevision,
    pub redacted_text: &'a str,
    pub source_updated_at: DateTime<Utc>,
    pub source_byte_count: usize,
    /// Eligible text measurements before the document-wide limit, when supplied by the reader.
    pub projection: Option<&'a omnis_ir::SearchProjection>,
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
    pub title_is_verified: bool,
    pub local_opener: LocalOpenerEvidence,
    pub withdrawn_title: Option<&'a str>,
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
    /// The selected rollout revision before opening its source.
    ObservedRevision(Option<TrajectoryIndexState>),
    /// A previous indexer recorded only the millisecond that contained the modification time.
    ObservedLegacy(DateTime<Utc>),
    /// The indexed document observed before reading a source whose update time can repeat.
    ObservedDocument(Option<[u8; 32]>),
    /// The document and catalog observed before opening one native session.
    Verified {
        state: [u8; 32],
        known_files: [Option<Uuid>; 3],
        source_byte_count: Option<usize>,
        full_or_provider_limited: bool,
        document_hash: Option<[u8; 32]>,
        sampled_file_bytes: Option<u64>,
        fully_scanned: bool,
    },
}

impl TrajectorySourceGuard {
    /// Whether a native reader identifies a search source by its file modification time.
    #[must_use]
    pub const fn uses_file_time(provider: Provider) -> bool {
        matches!(provider, Provider::Pi | Provider::OhMyPi)
    }

    /// A native catalog row and its physical source are verified together for these providers.
    #[must_use]
    pub const fn uses_verified_catalog(provider: Provider) -> bool {
        Self::uses_file_time(provider)
            || matches!(
                provider,
                Provider::Antigravity | Provider::AntigravityDesktop
            )
    }

    /// Compares the selected file with the document observed before the native read.
    #[must_use]
    pub fn matches_document_file(self, revision: SourceRevision) -> Option<bool> {
        let Self::Verified {
            known_files: [Some(stored), _, _],
            ..
        } = self
        else {
            return None;
        };
        revision
            .file
            .map(|file| stored == file || Some(stored) == revision.legacy_file)
    }

    /// Whether an index read returns the selected native file and its revision.
    #[must_use]
    pub const fn verifies_search_file(provider: Provider) -> bool {
        matches!(provider, Provider::Codex) || Self::uses_verified_catalog(provider)
    }

    /// Whether the stored document, catalog, or failure identifies another physical file for this session.
    #[must_use]
    pub fn has_competing_file(self, file: Option<Uuid>) -> bool {
        matches!((self, file), (Self::Verified { known_files, .. }, Some(file)) if known_files.into_iter().flatten().any(|known| known != file))
    }

    fn check_revision(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        document: Option<&TrajectoryDocument<'_>>,
    ) -> Result<()> {
        let Self::ObservedRevision(prior) = self else {
            return Ok(());
        };
        if let Some(receipt) = prior.and_then(|state| state.legacy_tail_receipt)
            && document_receipt(transaction, session)? != Some(receipt)
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let mut statement = transaction
            .prepare_cached(
                "SELECT source_updated_at, source_file_id, missing_ancestor_id, source_contents_id
                 FROM visible_trajectories WHERE provider = ?1 AND session_id = ?2",
            )
            .map_err(database_error)?;
        let current = statement
            .query_row(params![session.provider.to_string(), session.id], |row| {
                Ok((row.get::<_, i64>(0)?, source_revision_from_row(row, 1)?))
            })
            .optional()
            .map_err(database_error)?;
        let expected = prior.map(|state| {
            (
                state.source_updated_at.timestamp_millis(),
                state.source_revision,
            )
        });
        // A concurrent reader may still improve coverage of the same rollout.
        if current == expected
            || document.is_some_and(|read| {
                current
                    == Some((
                        read.source_updated_at.timestamp_millis(),
                        read.source_revision,
                    ))
            })
        {
            Ok(())
        } else {
            Err(StoreError::TrajectorySourceChanged)
        }
    }
    fn check(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        source: DateTime<Utc>,
    ) -> Result<()> {
        let prior = match self {
            Self::ObservedRevision(_) => return self.check_revision(transaction, session, None),
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
            .prepare_cached("SELECT source_updated_at, source_updated_submillis FROM visible_trajectories WHERE provider = ?1 AND session_id = ?2")
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
    /// Searchable bytes observed by the previous read before document truncation.
    pub source_byte_count: usize,
    /// Receipt of a native JSONL preview that needs a tail refresh.
    pub legacy_tail_receipt: Option<[u8; 32]>,
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
    pub document_version: u32,
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
                    source_file_id, source_contents_id FROM scoped_trajectory_failures
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
    workspace: Option<&'document CodexWorkspaceUpdate>,
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
        if document.projection.is_some_and(|projection| {
            !(document.indexed_byte_count..=document.source_byte_count)
                .contains(&projection.byte_count)
        }) || (document.source_complete && document.provider_limited)
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
            workspace: None,
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

    fn reuses_antigravity_chunks(&self, connection: &Connection) -> Result<bool> {
        let document = self.document;
        if !matches!(
            self.session.provider,
            Provider::Antigravity | Provider::AntigravityDesktop
        ) || document.origin != SessionTrajectoryOrigin::Native
            || document.source_revision.contents.is_none()
            || self.guard == TrajectorySourceGuard::NewerSource
        {
            return Ok(false);
        }
        connection
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM session_trajectories
                 WHERE provider = ?1 AND session_id = ?2 AND origin = 'native'
                   AND document_version BETWEEN 4 AND ?3 AND content_hash = ?4)",
            )
            .map_err(database_error)?
            .query_row(
                params![
                    self.session.provider.to_string(),
                    self.session.id,
                    document.document_version,
                    self.content_hash
                ],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    fn publish_verified_metadata(
        &self,
        transaction: &Transaction<'_>,
        metadata: &IndexedSession,
        home: Option<&Path>,
    ) -> Result<()> {
        if !matches_catalog_row(transaction, metadata)? {
            let mut upsert = prepare_indexed_session(transaction)?;
            write_indexed_session_row(&mut upsert, metadata, now_timestamp(), true)?;
        }
        if self.document.source_revision.file.is_some() {
            transaction
                .execute(
                    "DELETE FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2 AND (provider <> 'codex' OR codex_home=(SELECT home FROM current_codex_home))",
                    params![self.session.provider.to_string(), self.session.id],
                )
                .map_err(database_error)?;
        }
        if let Some(home) = home {
            record_discovery_home(transaction, &metadata.session, home)?;
            discovery::record_verified_document_home(
                transaction,
                &metadata.session,
                self.document.source_revision,
                home,
            )?;
            database_receipts::record_verified(transaction, self, home)?;
        }
        Ok(())
    }

    // A catalog rename or move can arrive in a preview of a previously verified full transcript.
    fn update_antigravity_catalog_time(&self, connection: &Connection) -> Result<Option<i64>> {
        let document = self.document;
        if !matches!(
            self.session.provider,
            Provider::Antigravity | Provider::AntigravityDesktop
        ) || !matches!(self.guard, TrajectorySourceGuard::Verified { .. })
        {
            return Ok(None);
        }
        let mut update = connection.prepare_cached(
            "UPDATE session_trajectories SET source_updated_at = ?1, source_updated_submillis = ?2
             WHERE provider = ?3 AND session_id = ?4 AND origin = 'native'
               AND document_version = ?5 AND source_file_id = ?6 AND source_contents_id = ?7
               AND (source_complete OR provider_limited) RETURNING id",
        ).map_err(database_error)?;
        update
            .query_row(
                params![
                    document.source_updated_at.timestamp_millis(),
                    source_submillis(self.session.provider, document.source_updated_at),
                    self.session.provider.to_string(),
                    self.session.id,
                    document.document_version,
                    document.source_revision.file.map(Uuid::into_bytes),
                    document.source_revision.contents.map(Uuid::into_bytes),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)
    }

    fn upsert_parent(
        &self,
        upsert_parent: &mut rusqlite::CachedStatement<'_>,
    ) -> Result<Option<i64>> {
        let revision = self.document.source_revision;
        upsert_parent
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
                    self.document.derived_title,
                    self.guard != TrajectorySourceGuard::NewerSource,
                    source_submillis(self.session.provider, self.document.source_updated_at),
                    self.document.provider_limited,
                    self.document.can_clear_derived_title(),
                    (self.verified_metadata.is_some()
                        && (self.source_complete || self.document.provider_limited))
                        || self.observed_full_source(),
                    revision.file.map(Uuid::into_bytes),
                    revision.missing_ancestor.map(Uuid::into_bytes),
                    revision.contents.map(Uuid::into_bytes),
                    self.verified_metadata.is_some(),
                    self.verified_metadata.is_some()
                        && revision.file.is_some()
                        && ((self.session.provider == Provider::Antigravity
                            && (self.guard.matches_document_file(revision) != Some(true)
                                || self
                                    .document
                                    .projection
                                    .is_some_and(
                                        |p| p.interior_damage.is_some() || p.verified_append
                                    )))
                            || (matches!(
                                self.session.provider,
                                Provider::Antigravity | Provider::AntigravityDesktop
                            ) && self.document.projection.is_some_and(|p| p.database_gap))),
                    self.document
                        .projection
                        .map(|p| i64::try_from(p.byte_count))
                        .transpose()
                        .map_err(|_| StoreError::InvalidSessionReference)?,
                    self.document.projection.map(|p| p.before_last_record),
                    self.document
                        .projection
                        .and_then(|p| p.sampled_file_bytes)
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| StoreError::InvalidSessionReference)?,
                    self.document
                        .projection
                        .and_then(|p| p.fully_scanned_bytes)
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| StoreError::InvalidSessionReference)?,
                    self.document.projection.and_then(|p| p.last_record_time),
                    self.is_jsonl_preview(),
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)
    }

    fn is_jsonl_preview(&self) -> bool {
        !self.source_complete
            && self.document.origin == SessionTrajectoryOrigin::Native
            && samples_jsonl_tail(self.session.provider)
    }

    fn should_publish(&self, transaction: &Transaction<'_>, home: Option<&Path>) -> Result<bool> {
        let revision = self.document.source_revision;
        let guard = if matches!(self.guard, TrajectorySourceGuard::ObservedRevision(_)) {
            self.guard
                .check_revision(transaction, self.session, Some(&self.document))
        } else {
            self.guard
                .check(transaction, self.session, self.document.source_updated_at)
        };
        // A stored sample can already match this write when a full read fails.
        // Check the failure before accepting either the sample or an idempotent write.
        let failure = if self.session.provider != Provider::Codex
            && self.document.origin == SessionTrajectoryOrigin::Native
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
        guard?;
        if self.verified_metadata.is_some()
            && matches!(
                self.session.provider,
                Provider::Antigravity | Provider::AntigravityDesktop
            )
            && self.document.origin == SessionTrajectoryOrigin::Native
            && home.is_some()
            && revision.file.is_some()
            && revision.contents.is_some()
            && self.document.projection.is_some_and(|p| p.database_gap)
        {
            return Ok(true);
        }
        Ok(!self.retains_verified_antigravity_text(transaction, home)?)
    }

    fn retains_verified_antigravity_text(
        &self,
        transaction: &Transaction<'_>,
        home: Option<&Path>,
    ) -> Result<bool> {
        let document = self.document;
        if self.session.provider != Provider::Antigravity
            || document.origin != SessionTrajectoryOrigin::Native
            || self.complete
        {
            return Ok(false);
        }
        let TrajectorySourceGuard::Verified {
            source_byte_count: Some(previous_bytes),
            ..
        } = self.guard
        else {
            return Ok(false);
        };
        if previous_bytes == 0 {
            return Ok(false);
        }
        if document.indexed_byte_count != 0
            && (!document
                .truncation_strategy
                .starts_with("source_incomplete")
                || previous_bytes <= document.source_byte_count)
        {
            return Ok(false);
        }
        let prior = transaction.prepare_cached(
            "SELECT doc.id, doc.indexed_byte_count, doc.truncation_strategy, doc.source_file_id, doc.projected_byte_count, doc.projected_prefix_hash, doc.projected_record_prefix_hash FROM session_trajectories AS doc
             LEFT JOIN native_search_document_homes AS origin ON origin.trajectory_id=doc.id AND origin.source_file=doc.source_file_id
             WHERE doc.provider=?1 AND doc.session_id=?2 AND doc.origin='native' AND doc.source_file_id IN (?3,?6)
               AND doc.indexed_byte_count>0 AND doc.indexed_byte_count<=?4
               AND (origin.source_home IS NULL OR (origin.verification=1 AND origin.source_home=?5))"
        ).map_err(database_error)?.query_row(params![
            self.session.provider.to_string(), self.session.id, document.source_revision.file.map(Uuid::into_bytes),
            i64::try_from(MAX_TRAJECTORY_WRITE_BATCH_BYTES).expect("batch fits i64"),
            home.map(|home| home.as_os_str().as_encoded_bytes()), document.source_revision.legacy_file.map(Uuid::into_bytes)
        ], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)? as usize, row.get::<_, String>(2)?, optional_uuid(row, 3)?, row.get::<_, Option<i64>>(4)?, row.get::<_, Option<Vec<u8>>>(5)?, row.get::<_, Option<Vec<u8>>>(6)?))).optional().map_err(database_error)?;
        let Some((id, length, strategy, old_file, old_projected, event_prefix, record_prefix)) =
            prior
        else {
            return Ok(false);
        };
        let old_prefix = event_prefix.as_deref().or(record_prefix.as_deref());
        let same_prefix = document
            .projection
            .is_some_and(|proof| old_prefix == Some(&proof.digest));
        let retain = if document
            .projection
            .is_some_and(|p| p.interior_damage.is_some())
        {
            // Uncapped gaps can combine the old record with newly observed text.
            // A capped prefix can protect the old document only when all observed text matches.
            same_prefix
                && (strategy.ends_with("document_head_tail")
                    || document.truncation_strategy != "source_incomplete")
        } else if document.indexed_byte_count == 0 || same_prefix {
            true
        } else {
            let window = ProjectionWindow::for_documents(
                &strategy,
                old_projected,
                previous_bytes,
                &document,
            );
            if !matches!(window, ProjectionWindow::Uncapped)
                && event_prefix.is_none()
                && record_prefix.is_some()
                && document.projection.is_some()
            {
                return Ok(false);
            }
            stored_text_contains_projection(
                transaction,
                id,
                length,
                document.redacted_text,
                window,
                document.projection.zip(event_prefix.as_deref()),
            )?
        };
        if retain && old_file != document.source_revision.file {
            let old = old_file.map(Uuid::into_bytes);
            let current = document.source_revision.file.map(Uuid::into_bytes);
            transaction.execute("UPDATE session_trajectories SET source_file_id=?2 WHERE id=?1 AND source_file_id=?3", params![id, current, old]).map_err(database_error)?;
            transaction.execute("UPDATE native_search_document_homes SET source_file=?2 WHERE trajectory_id=?1 AND source_file=?3", params![id, current, old]).map_err(database_error)?;
            if let Some(home) = home {
                let changed = transaction.execute(
                    "UPDATE session_index SET source_file_id=?3 WHERE provider=?1 AND session_id=?2 AND source_file_id=?4
                     AND EXISTS (SELECT 1 FROM native_catalog_homes WHERE provider=?1 AND session_id=?2 AND verified AND home=?5)",
                    params![self.session.provider.to_string(), self.session.id, current, old, home.as_os_str().as_encoded_bytes()],
                ).map_err(database_error)?;
                if changed > 0 {
                    record_discovery_home(transaction, self.session, home)?;
                }
            }
        }
        Ok(retain)
    }

    fn legacy_sample_update(
        &self,
        transaction: &Transaction<'_>,
        home: Option<&Path>,
    ) -> Result<LegacySampleUpdate> {
        let doc = self.document;
        let Some(projection) = doc
            .projection
            .filter(|p| p.torn_final || p.legacy_damage_sample.is_some())
        else {
            return Ok(LegacySampleUpdate::Observed);
        };
        if self.guard.matches_document_file(doc.source_revision) != Some(true)
            || self.verified_metadata.is_none()
        {
            return Ok(LegacySampleUpdate::Observed);
        }
        let Some((digest, sampled)) = projection.legacy_damage_sample else {
            let unchanged = transaction.query_row(
                "SELECT EXISTS (SELECT 1 FROM session_trajectories d JOIN native_search_document_homes h
                 ON h.trajectory_id=d.id AND h.source_file=d.source_file_id
                 WHERE d.provider='antigravity' AND d.session_id=?1 AND d.origin='native'
                 AND h.verification=1 AND h.source_home=?2 AND d.source_file_id IN (?3,?4)
                 AND d.recovered_gap_start=d.observed_text_bytes AND d.observed_text_hash=?5)",
                params![self.session.id, home.map(|h| h.as_os_str().as_encoded_bytes()),
                    doc.source_revision.file.map(Uuid::into_bytes), doc.source_revision.legacy_file.map(Uuid::into_bytes),self.content_hash],
                |row|row.get::<_,bool>(0)).map_err(database_error)?;
            return Ok(if unchanged {
                LegacySampleUpdate::Keep
            } else {
                LegacySampleUpdate::Observed
            });
        };
        let prior = transaction
            .query_row(
                "SELECT d.id,d.indexed_byte_count,d.projected_byte_count,
                    coalesce(d.projected_record_prefix_hash,d.projected_prefix_hash)
             FROM session_trajectories d JOIN native_search_document_homes h
             ON h.trajectory_id=d.id AND h.source_file=d.source_file_id
             WHERE d.provider='antigravity' AND d.session_id=?1 AND d.origin='native'
               AND h.verification=1 AND h.source_home=?2 AND d.source_file_id IN (?3,?4)
               AND d.indexed_byte_count<=?5",
                params![
                    self.session.id,
                    home.map(|h| h.as_os_str().as_encoded_bytes()),
                    doc.source_revision.file.map(Uuid::into_bytes),
                    doc.source_revision.legacy_file.map(Uuid::into_bytes),
                    i64::try_from(2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT)
                        .expect("bounded document")
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)? as usize,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        let Some((id, length, count, prefix)) = prior else {
            return Ok(LegacySampleUpdate::Keep);
        };
        let Some(previous) = stored_projection_text(transaction, id, length)? else {
            return Ok(LegacySampleUpdate::Keep);
        };
        if projection.torn_final && prefix.as_deref() == Some(&digest) {
            let missing = count
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| n.checked_sub(sampled));
            if let Some(suffix) = missing.and_then(|n| previous.get(length.checked_sub(n)?..))
                && !suffix.is_empty()
                && doc.truncation_strategy == "source_incomplete"
                && doc.redacted_text.len() + suffix.len()
                    <= 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT
            {
                return Ok(LegacySampleUpdate::Restore(RecoveredRecord {
                    text: [doc.redacted_text, suffix].concat(),
                    start: doc.redacted_text.len(),
                    evidence: RecordEvidence {
                        time: None,
                        anchor: None,
                    },
                }));
            }
        }
        if doc.truncation_strategy == "source_incomplete"
            && count.and_then(|n| usize::try_from(n).ok()) == Some(length)
            && let (Some(digest), Some(starts)) =
                (prefix.as_deref(), projection.interior_damage.as_deref())
            && let Some(record) = legacy_record_at_gap(&previous, digest, doc.redacted_text, starts)
        {
            return Ok(LegacySampleUpdate::Restore(record));
        }
        let mut observed = doc.redacted_text.split("\n\n");
        if previous
            .split("\n\n")
            .all(|old| observed.any(|new| new == old))
        {
            return Ok(LegacySampleUpdate::Observed);
        }
        Ok(LegacySampleUpdate::Keep)
    }

    fn recover_interior_record(
        &self,
        transaction: &Transaction<'_>,
        home: Option<&Path>,
    ) -> Result<Option<RecoveredRecord>> {
        let doc = self.document;
        let Some(starts) = doc.projection.and_then(|p| p.interior_damage.as_deref()) else {
            return Ok(None);
        };
        if self.session.provider != Provider::Antigravity
            || doc.truncation_strategy != "source_incomplete"
            || self.guard.matches_document_file(doc.source_revision) != Some(true)
            || self.verified_metadata.is_none()
        {
            return Ok(None);
        }
        let Some(home) = home else {
            return Ok(None);
        };
        let mut statement = transaction.prepare_cached(
            "SELECT d.id, d.indexed_byte_count, d.source_complete AND d.truncation_strategy IN ('none','event_head_tail'),
                    coalesce(d.projected_record_prefix_hash,d.projected_prefix_hash),
                    d.recovered_gap_start, d.observed_text_bytes, d.verified_record_time, d.recovered_anchor_bytes
             FROM session_trajectories d
             JOIN native_search_document_homes h ON h.trajectory_id=d.id AND h.source_file=d.source_file_id
             WHERE d.provider=?1 AND d.session_id=?2 AND d.origin='native'
               AND h.verification=1 AND h.source_home=?3
               AND d.source_file_id IN (?4,?5) AND d.indexed_byte_count<=?6"
        ).map_err(database_error)?;
        let prior = statement
            .query_row(
                params![
                    self.session.provider.to_string(),
                    self.session.id,
                    home.as_os_str().as_encoded_bytes(),
                    doc.source_revision.file.map(Uuid::into_bytes),
                    doc.source_revision.legacy_file.map(Uuid::into_bytes),
                    i64::try_from(2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT)
                        .expect("document cap fits i64"),
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)? as usize,
                        row.get::<_, bool>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Option<u32>>(4)?.map(|n| n as usize),
                        row.get::<_, Option<u32>>(5)?.map(|n| n as usize),
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<u32>>(7)?.map(|n| n as usize),
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        let Some((id, length, full, old_prefix, old_gap, observed_bytes, time, anchor)) = prior
        else {
            return Ok(None);
        };
        let candidate = if let (Some(start), Some(observed)) = (old_gap, observed_bytes) {
            starts
                .binary_search(&start)
                .ok()
                .and_then(|_| Some((start, length.checked_sub(observed)?)))
        } else if full {
            old_prefix.as_deref().and_then(|digest| {
                let start = verified_gap_start(doc.redacted_text, starts, digest)?;
                Some((start, length.checked_sub(start)?))
            })
        } else {
            None
        };
        let Some((start, missing)) = candidate else {
            return Ok(None);
        };
        let Some(previous) = stored_projection_text(transaction, id, length)? else {
            return Ok(None);
        };
        Ok(recover_missing_record_text(
            doc.redacted_text,
            &previous,
            start,
            missing,
            old_gap.is_some(),
            RecordEvidence { time, anchor },
            doc.projection
                .map_or(&[][..], |p| p.dated_suffix_records.as_slice()),
        ))
    }

    fn matches_stored(&self, connection: &Connection) -> Result<bool> {
        let doc = self.document;
        connection
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM visible_trajectories
                    WHERE provider = ?1 AND session_id = ?2 AND content_hash = ?3
                      AND source_updated_at = ?4 AND source_updated_submillis = ?5
                      AND source_complete = ?6 AND provider_limited = ?7 AND complete = ?8
                      AND source_byte_count = ?9 AND indexed_byte_count = ?10
                      AND truncation_strategy = ?11 AND origin = ?12 AND document_version = ?13
                      AND (NOT ?14 OR derived_title IS ?15)
                      AND title_authoritative IS ?16 AND source_file_id IS ?17
                      AND missing_ancestor_id IS ?18 AND source_contents_id IS ?19
                      AND (?20 IS NULL OR (projected_byte_count IS ?20 AND projected_record_prefix_hash IS ?21 AND sampled_file_bytes IS ?22 AND fully_scanned_bytes IS ?23 AND verified_record_time IS ?24 AND projected_prefix_hash IS NULL))
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
                    doc.projection
                        .map(|p| i64::try_from(p.byte_count))
                        .transpose()
                        .map_err(|_| StoreError::InvalidSessionReference)?,
                    doc.projection.map(|p| p.before_last_record),
                    doc.projection.and_then(|p| p.sampled_file_bytes).map(i64::try_from).transpose().map_err(|_| StoreError::InvalidSessionReference)?,
                    doc.projection.and_then(|p| p.fully_scanned_bytes).map(i64::try_from).transpose().map_err(|_| StoreError::InvalidSessionReference)?,
                    doc.projection.and_then(|p| p.last_record_time),
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
        || !TrajectorySourceGuard::verifies_search_file(session.provider)
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

const DOCUMENT_RECEIPT_FIELDS: &str =
    "json_array(id, indexed_at, hex(content_hash), source_updated_at,
         source_updated_submillis, source_complete, provider_limited, complete,
         document_version, source_byte_count, indexed_byte_count, truncation_strategy,
         origin, derived_title, title_authoritative, hex(source_file_id), hex(missing_ancestor_id),
         hex(source_contents_id), tail_reader_version, projected_byte_count, hex(projected_prefix_hash), hex(projected_record_prefix_hash), sampled_file_bytes, fully_scanned_bytes, verified_record_time, recovered_anchor_bytes,
         recovered_gap_start, observed_text_bytes, hex(observed_text_hash),
         (SELECT hex(observed_contents) FROM antigravity_search_gaps WHERE trajectory_id=session_trajectories.id),
         (SELECT json_array(document_version,hex(content_hash),derived_title,source_updated_at) FROM antigravity_gap_projections
          WHERE trajectory_id=session_trajectories.id))";

fn document_receipt_sql() -> String {
    format!(
        "SELECT {DOCUMENT_RECEIPT_FIELDS} FROM visible_trajectories AS session_trajectories WHERE provider = ?1 AND session_id = ?2"
    )
}

fn trajectory_state_columns() -> String {
    format!("source_updated_at, source_complete, document_version, source_updated_submillis,
             provider_limited, title_authoritative, source_file_id, missing_ancestor_id, source_contents_id, source_byte_count,
             CASE WHEN origin = 'native' AND source_complete = 0 AND tail_reader_version = 0
                  THEN {DOCUMENT_RECEIPT_FIELDS} END")
}

fn document_receipt(connection: &Connection, session: &SessionRef) -> Result<Option<[u8; 32]>> {
    connection
        .prepare_cached(&document_receipt_sql())
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
    let document = document_receipt_sql();
    let sql = format!(
        "SELECT json_array(?1, ?2, ({document}),
            CASE WHEN catalog.provider IS NOT NULL THEN json_array(catalog.title, catalog.project_path,
                catalog.git_branch, catalog.created_at, catalog.updated_at, catalog.updated_at_approximate,
                catalog.event_count, catalog.source_updated_submillis, hex(catalog.source_file_id),
                hex(catalog.source_contents_id)) END,
            CASE WHEN failed.provider IS NOT NULL THEN json_array(failed.source_updated_at,
                failed.source_updated_submillis, failed.document_version, hex(failed.source_file_id),
                hex(failed.source_contents_id)) END),
            doc.source_file_id, catalog.source_file_id,
            CASE WHEN failed.source_contents_id IS NOT NULL THEN failed.source_file_id END,
            coalesce(doc.observed_text_bytes,doc.source_byte_count), coalesce(doc.source_complete OR doc.provider_limited, 0),
            CASE WHEN doc.provider = 'antigravity' AND NOT doc.provider_limited
                      AND length(doc.content_hash) = 32 THEN coalesce(doc.observed_text_hash,doc.content_hash) END, doc.sampled_file_bytes,
            doc.fully_scanned_bytes IS NOT NULL OR doc.recovered_gap_start IS NOT NULL
         FROM (SELECT ?1 AS provider, ?2 AS session_id) AS selected
         LEFT JOIN visible_trajectories AS doc USING (provider, session_id)
         LEFT JOIN scoped_session_index AS catalog USING (provider, session_id)
         LEFT JOIN scoped_trajectory_failures AS failed USING (provider, session_id)"
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
                full_or_provider_limited: row.get(5)?,
                document_hash: row.get(6)?,
                fully_scanned: row.get(8)?,
                sampled_file_bytes: row
                    .get::<_, Option<i64>>(7)?
                    .and_then(|n| u64::try_from(n).ok()),
                source_byte_count: row
                    .get::<_, Option<i64>>(4)?
                    .map(usize::try_from)
                    .transpose()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(4, Type::Integer, Box::new(error))
                    })?,
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
            "SELECT EXISTS (SELECT 1 FROM scoped_session_index WHERE provider = ?1 AND session_id = ?2
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
    codex_home: Option<String>,
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
            codex_home: active_codex_home(),
        };
        store.initialize_schema()?;
        initialize_trajectory_scope(&store.connection.borrow(), store.codex_home.as_deref())?;
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
                SELECT 1 FROM scoped_session_index AS catalog
                JOIN scoped_trajectory_failures AS failed USING (provider, session_id)
                WHERE catalog.provider = ?1 AND catalog.session_id = ?2
                  AND failed.source_file_id IS NOT NULL AND failed.source_contents_id IS NOT NULL
                  AND (failed.document_version < ?5
                       OR (failed.source_file_id, failed.source_contents_id) IS NOT (?3, ?4))
                  AND ((catalog.source_file_id, catalog.source_contents_id) IS (?3, ?4)
                       OR (catalog.source_file_id IS NULL AND EXISTS (
                            SELECT 1 FROM visible_trajectories AS doc
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
        let catalog = if visible_only {
            "observed_session_index"
        } else {
            "scoped_session_index"
        };
        let mut sql = format!(
            "SELECT provider, session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count, source_updated_submillis,
                    source_file_id, source_contents_id
             FROM {catalog} AS catalog WHERE 1",
        );
        if provider.is_some() {
            sql.push_str(" AND catalog.provider = ?1");
        }
        if visible_only {
            sql.push_str(" AND ");
            sql.push_str(VISIBLE_CATALOG_ROW);
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

    fn codex_home_for(&self, provider: Provider) -> Option<&str> {
        self.codex_home
            .as_deref()
            .filter(|_| provider == Provider::Codex)
    }

    fn index_check_key(&self, provider: Provider) -> String {
        self.codex_home_for(provider)
            .map_or_else(|| provider.to_string(), |home| format!("codex:{home}"))
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

    /// Publishes a listed Codex replacement if the previous catalog row and selected file still match.
    /// The previous document remains available if discovery selects its rollout again.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed catalog operation.
    pub fn refresh_replaced_codex_session(
        &self,
        previous: &IndexedSession,
        selected: &IndexedSession,
        verify: impl FnOnce() -> bool,
    ) -> Result<()> {
        if selected.session.provider != Provider::Codex || previous.session != selected.session {
            return Err(StoreError::InvalidSessionReference);
        }
        validate_session_ref(&selected.session)?;
        let previous_revision = previous.source_revision.unwrap_or_default();
        let selected_revision = selected.source_revision.unwrap_or_default();
        if previous_revision.file.is_none()
            || selected_revision.file.is_none()
            || previous_revision.matches_file_contents(selected_revision)
        {
            return Ok(());
        }
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        if catalog_session(&transaction, &selected.session, true)?.as_ref() != Some(previous) {
            return Ok(());
        }
        Self::write_indexed_session(&transaction, selected)?;
        if verify() {
            transaction.commit().map_err(database_error)?;
        }
        Ok(())
    }

    fn write_indexed_session(connection: &Connection, session: &IndexedSession) -> Result<()> {
        validate_session_ref(&session.session)?;
        let mut upsert = prepare_indexed_session(connection)?;
        write_indexed_session_row(&mut upsert, session, now_timestamp(), false)
    }

    /// Records the workspace found in a rollout and makes the session available to cached discovery.
    /// The adapter checks the source stamp before it supplies this observation.
    ///
    /// # Errors
    ///
    /// Returns an error if the source lacks an exact timestamp, its metadata is invalid, or persistence fails.
    pub fn record_codex_workspace(
        &self,
        session: &IndexedSession,
        home: &Path,
        source_path: &Path,
        source_stamp: &str,
    ) -> Result<()> {
        validate_session_ref(&session.session)?;
        let home = home
            .to_str()
            .filter(|home| !home.is_empty())
            .ok_or(StoreError::InvalidWorkspaceRoot)?;
        if session.session.provider != Provider::Codex
            || source_stamp.is_empty()
            || session.updated_at_approximate
        {
            return Err(StoreError::InvalidSessionReference);
        }
        let updated_at = session
            .updated_at
            .ok_or(StoreError::InvalidSessionReference)?
            .timestamp_millis();
        let source_path = source_path
            .to_str()
            .filter(|path| !path.is_empty())
            .ok_or(StoreError::InvalidWorkspaceRoot)?;
        let project_path = session
            .project_path
            .as_deref()
            .map(|path| path.to_str().ok_or(StoreError::InvalidWorkspaceRoot))
            .transpose()?;
        let event_count =
            i64::try_from(session.event_count).map_err(|_| StoreError::InvalidSessionReference)?;
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction.execute(
            "UPDATE session_index AS i SET project_path = NULL, git_branch = NULL
             WHERE provider = 'codex' AND session_id = ?1 AND codex_home = ?2 AND EXISTS (
                 SELECT 1 FROM codex_workspace_observations AS w WHERE w.session_id = i.session_id
                 AND w.provider_home = ?2 AND w.source_stamp != ?3 AND
                     (w.source_file_id, w.source_contents_id) IS (i.source_file_id, i.source_contents_id) AND
                     (w.project_path, w.git_branch, w.source_updated_at) IS (i.project_path, i.git_branch, i.updated_at))",
            params![session.session.id, home, source_stamp],
        ).map_err(database_error)?;
        let revision = session.source_revision.unwrap_or_default();
        let file = revision.file.map(Uuid::into_bytes);
        let contents = revision.contents.map(Uuid::into_bytes);
        let changed = transaction.execute(
            "INSERT INTO codex_workspace_observations
                 (session_id, source_path, source_stamp, source_updated_at, project_path, git_branch,
                  provider_home, title, created_at, event_count, source_file_id, source_contents_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (provider_home, session_id) DO UPDATE SET
                 source_path = excluded.source_path, source_stamp = excluded.source_stamp,
                 source_updated_at = excluded.source_updated_at, project_path = excluded.project_path,
                 git_branch = excluded.git_branch, title = excluded.title, created_at = excluded.created_at,
                 event_count = excluded.event_count, source_file_id = excluded.source_file_id, source_contents_id = excluded.source_contents_id
             WHERE (source_path, source_stamp, source_updated_at, project_path, git_branch, title, created_at, event_count, source_file_id, source_contents_id)
                IS NOT (excluded.source_path, excluded.source_stamp, excluded.source_updated_at,
                        excluded.project_path, excluded.git_branch, excluded.title, excluded.created_at, excluded.event_count, excluded.source_file_id, excluded.source_contents_id)",
            params![session.session.id, source_path, source_stamp, updated_at, project_path, session.git_branch,
                    home, session.title, session.created_at.map(|t| t.timestamp_millis()), event_count, file, contents],
        ).map_err(database_error)?;
        if changed != 0 {
            transaction.execute(
                "DELETE FROM session_index WHERE provider = 'codex' AND session_id = ?1 AND codex_home = ?2
                 AND (source_file_id IS NULL OR (source_file_id,source_contents_id) IS (?4,?5))
                 AND (updated_at IS NOT ?3 OR updated_at_approximate != 0)",
                params![session.session.id, home, updated_at,file,contents],
            ).map_err(database_error)?;
        }
        transaction.commit().map_err(database_error)
    }

    /// Returns all observed Codex workspaces for a provider discovery pass.
    ///
    /// # Errors
    ///
    /// Returns an error if the observations cannot be read.
    pub fn indexed_codex_workspaces(
        &self,
        home: &Path,
    ) -> Result<HashMap<String, IndexedCodexWorkspace>> {
        let connection = self.connection.borrow();
        let mut statement = connection.prepare(
            "SELECT source_path, source_stamp, project_path, git_branch, session_id FROM codex_workspace_observations WHERE provider_home = ?1",
        ).map_err(database_error)?;
        statement
            .query_map([home.to_str()], |row| {
                Ok((row.get(4)?, codex_workspace_from_row(row)?))
            })
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    /// Returns the observation for a selected Codex session.
    ///
    /// # Errors
    ///
    /// Returns an error if the observation cannot be read.
    pub fn indexed_codex_workspace(
        &self,
        home: &Path,
        id: &str,
    ) -> Result<Option<IndexedCodexWorkspace>> {
        self.connection
            .borrow()
            .query_row(
                "SELECT source_path, source_stamp, project_path, git_branch
             FROM codex_workspace_observations WHERE session_id = ?1 AND provider_home = ?2",
                params![id, home.to_str()],
                codex_workspace_from_row,
            )
            .optional()
            .map_err(database_error)
    }

    /// Removes a stale observation if the stored stamp still matches.
    ///
    /// # Errors
    ///
    /// Returns an error if the observation cannot be removed.
    pub fn forget_codex_workspace(&self, home: &Path, id: &str, old_stamp: &str) -> Result<()> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        transaction.execute(
            "UPDATE session_index SET project_path = NULL, git_branch = NULL
             WHERE provider = 'codex' AND session_id = ?1 AND codex_home = ?3 AND EXISTS (
                 SELECT 1 FROM codex_workspace_observations AS w
                 WHERE w.session_id = ?1 AND w.source_stamp = ?2 AND w.provider_home = ?3 AND
                       (w.source_file_id, w.source_contents_id) IS (session_index.source_file_id, session_index.source_contents_id) AND
                       (w.project_path, w.git_branch, w.source_updated_at)
                       IS (session_index.project_path, session_index.git_branch, session_index.updated_at))",
            params![id, old_stamp, home.to_str()],
        ).map_err(database_error)?;
        transaction.execute(
                "DELETE FROM codex_workspace_observations WHERE session_id = ?1 AND source_stamp = ?2 AND provider_home = ?3",
                params![id, old_stamp, home.to_str()],
        ).map_err(database_error)?;
        transaction.commit().map_err(database_error)
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
        remove_codex_workspace(&transaction, session, self.codex_home.as_deref())?;
        transaction
            .execute(
                "DELETE FROM session_index WHERE provider = ?1 AND session_id = ?2
                 AND (provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home))",
                params![provider, session.id],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "DELETE FROM session_trajectories WHERE provider = ?1 AND session_id = ?2
                 AND (provider <> 'codex' OR (origin = 'native' AND codex_home = (SELECT home FROM current_codex_home)))",
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
            "DELETE FROM session_index WHERE provider = ?1 AND session_id = ?2
                 AND (provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home))",
            "DELETE FROM session_trajectories WHERE provider = ?1 AND session_id = ?2
                 AND (provider <> 'codex' OR (origin = 'native' AND codex_home = (SELECT home FROM current_codex_home)))",
            "UPDATE session_bindings SET is_current = 0
             WHERE provider = ?1 AND session_id = ?2 AND is_current = 1",
        ] {
            transaction
                .execute(statement, params![provider, session.id])
                .map_err(database_error)?;
        }
        remove_codex_workspace(&transaction, session, self.codex_home.as_deref())?;
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
                params![self.index_check_key(provider)],
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
                params![self.index_check_key(provider)],
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
                params![self.index_check_key(provider), now_timestamp()],
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
        let unchanged = complete
            && provider_index_matches(
                &connection,
                provider,
                sessions,
                &self.index_check_key(provider),
            )?;
        let transaction = immediate_transaction(&mut connection)?;
        if let Some(prior) = observation
            && prior.generation != catalog_generation(&transaction, provider)?
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let provider_name = provider.to_string();
        let codex_home = self.codex_home_for(provider);
        let indexed_at = now_timestamp();
        if !unchanged {
            write_discovered_sessions(
                &transaction,
                provider,
                sessions,
                complete,
                indexed_at,
                &self.index_check_key(provider),
            )?;
        }
        transaction
            .execute(
                "
                INSERT INTO session_index_checks (provider, checked_at)
                VALUES (?1, ?2)
                ON CONFLICT (provider) DO UPDATE SET checked_at = excluded.checked_at
                ",
                params![self.index_check_key(provider), indexed_at],
            )
            .map_err(database_error)?;
        if complete {
            prune_stale_native_trajectories(&transaction, &provider_name, codex_home)?;
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
                title_is_verified: false,
                local_opener: crate::LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                source_revision: SourceRevision::default(),
                redacted_text,
                source_updated_at,
                source_byte_count,
                projection: None,
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
            &HashMap::new(),
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
            &HashMap::new(),
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
        self.upsert_selected_trajectory_documents_in_homes(documents, &HashMap::new(), verify)
    }

    /// Publishes documents and the native homes that verified their catalog metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if a selected source changed or the batch could not be written.
    pub fn upsert_selected_trajectory_documents_in_homes<'s, 'd>(
        &self,
        documents: impl ExactSizeIterator<Item = SelectedTrajectoryDocument<'s, 'd>>,
        homes: &HashMap<Provider, PathBuf>,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
    ) -> Result<usize> {
        let max_indexed_bytes = if documents.len() == 1 {
            usize::MAX
        } else {
            MAX_TRAJECTORY_WRITE_BATCH_BYTES
        };
        self.upsert_trajectory_documents_with_limits(documents, max_indexed_bytes, homes, verify)
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
        homes: &HashMap<Provider, PathBuf>,
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

        self.publish_trajectory_documents(&prepared, remaining_bytes, homes, verify, None)
    }

    fn publish_trajectory_documents(
        &self,
        prepared: &[PreparedTrajectoryWrite<'_, '_>],
        mut remaining: usize,
        homes: &HashMap<Provider, PathBuf>,
        verify: impl Fn(&SessionRef, SourceRevision) -> bool,
        selected_metadata: Option<SelectedMetadata<'_>>,
    ) -> Result<usize> {
        let mut connection = self.connection.borrow_mut();
        let transaction = immediate_transaction(&mut connection)?;
        let mut published = HashSet::with_capacity(prepared.len());
        let mut rewritten = Vec::with_capacity(prepared.len());
        let mut upsert_parent = transaction
            .prepare_cached(UPSERT_TRAJECTORY_PARENT_SQL)
            .map_err(database_error)?;
        for write in prepared {
            let home = homes.get(&write.session.provider).map(PathBuf::as_path);
            if !write.should_publish(&transaction, home)? {
                continue;
            }
            let recovered = match write.legacy_sample_update(&transaction, home)? {
                LegacySampleUpdate::Keep => continue,
                LegacySampleUpdate::Observed => {
                    write.recover_interior_record(&transaction, home)?
                }
                LegacySampleUpdate::Restore(record) => Some(record),
            };
            let reuse_chunks = write.reuses_antigravity_chunks(&transaction)?;
            let trajectory_id = if write.session.provider == Provider::Codex {
                write.publish_codex_parent(&transaction, selected_metadata)?
            } else {
                write.upsert_parent(&mut upsert_parent)?
            };
            if let Some(metadata) = write.verified_metadata {
                if trajectory_id.is_none() && !write.matches_stored(&transaction)? {
                    if write.source_complete || write.document.provider_limited {
                        return Err(StoreError::TrajectorySourceChanged);
                    }
                    let Some(id) = write.update_antigravity_catalog_time(&transaction)? else {
                        // An older full document keeps its catalog until the new source is verified.
                        continue;
                    };
                    published.insert(id);
                }
                write.publish_verified_metadata(
                    &transaction,
                    metadata,
                    homes.get(&metadata.session.provider).map(PathBuf::as_path),
                )?;
            }
            if let Some(trajectory_id) = trajectory_id {
                if write.is_jsonl_preview() {
                    transaction
                        .execute(
                            "UPDATE session_trajectories SET tail_reader_version = 1 WHERE id = ?1",
                            [trajectory_id],
                        )
                        .map_err(database_error)?;
                }
                // A later write of the same session replaces an earlier one in this batch.
                published.insert(trajectory_id);
                if !reuse_chunks || recovered.is_some() {
                    let text = if let Some(record) = recovered {
                        remaining = remaining
                            .checked_sub(record.text.len() - write.document.indexed_byte_count)
                            .ok_or(StoreError::TrajectoryBatchTooLarge)?;
                        record.store(&transaction, trajectory_id)?;
                        std::borrow::Cow::Owned(record.text)
                    } else {
                        std::borrow::Cow::Borrowed(write.document.redacted_text)
                    };
                    rewritten.retain(|(other, _)| *other != trajectory_id);
                    rewritten.push((trajectory_id, text));
                }
            } else if write.document.origin == SessionTrajectoryOrigin::ImportedBundle {
                transaction
                    .execute(
                        "UPDATE session_trajectories SET protected_by_bundle = 1
                         WHERE provider = ?1 AND session_id = ?2 AND (provider <> 'codex' OR origin = 'imported_bundle')",
                        params![write.session.provider.to_string(), write.session.id],
                    )
                    .map_err(database_error)?;
            }
        }
        drop(upsert_parent);
        let chunks: Vec<_> = rewritten
            .iter()
            .map(|(id, text)| (*id, text.as_ref()))
            .collect();
        replace_trajectory_chunks(&transaction, &chunks)?;
        for write in prepared {
            if write.document.origin == SessionTrajectoryOrigin::Native
                && write.document.source_revision.file.is_some()
                && !verify(write.session, write.document.source_revision)
            {
                return Err(StoreError::TrajectorySourceChanged);
            }
        }
        if let Some(metadata) = selected_metadata {
            metadata.publish(&transaction)?;
        }
        transaction.commit().map_err(database_error)?;
        Ok(published.len())
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
                    FROM visible_trajectories
                    WHERE provider = ?1
                      AND session_id = ?2
                      AND complete = 1
                      AND source_updated_at >= ?3 AND (provider <> 'codex' OR source_updated_at = ?3) AND (NOT ?4 OR (source_updated_at = ?3 AND source_updated_submillis = ?5))
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
                    SELECT 1 FROM visible_trajectories
                    WHERE provider = ?1 AND session_id = ?2
                      AND source_complete = 1 AND source_updated_at >= ?3 AND (provider <> 'codex' OR source_updated_at = ?3) AND document_version >= ?4
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
        cached_trajectory_state(&self.connection.borrow(), session)
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
            .prepare(&format!(
                "SELECT provider, session_id, {} FROM visible_trajectories AS session_trajectories",
                trajectory_state_columns()
            ))
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
                        source_file_id, source_contents_id FROM trajectory_index_failures WHERE provider <> 'codex' OR codex_home = ?1",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([self.codex_home.as_deref()], |row| {
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
                     provider, session_id, source_updated_at, document_version, failed_at, source_updated_submillis, source_file_id, source_contents_id, codex_home
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT DO UPDATE SET
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
                    self.codex_home_for(session.provider),
                ],
            )
            .map_err(database_error)?;
        let revision = SourceRevision {
            legacy_file: None,
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
        clear_observed_failure(&self.connection.borrow(), session, *failure)
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
                "DELETE FROM trajectory_index_failures WHERE provider = ?1 AND session_id = ?2
                 AND (provider <> 'codex' OR codex_home = ?3)",
                params![
                    session.provider.to_string(),
                    session.id,
                    self.codex_home_for(session.provider)
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    /// Returns native documents that need a catalog or database gap check.
    ///
    /// # Errors
    ///
    /// Returns an error if the index cannot be read.
    pub fn conflicting_file_catalog_rows(
        &self,
        provider: Provider,
    ) -> Result<std::collections::HashSet<SessionRef>> {
        let connection = self.connection.borrow();
        let sql = if matches!(
            provider,
            Provider::Antigravity | Provider::AntigravityDesktop
        ) {
            // An exact restore can match the primary receipt while a later observation still records a gap.
            format!(
                "SELECT trajectories.session_id FROM visible_trajectories AS trajectories
                 {SEARCH_GAP_JOIN}
                 WHERE trajectories.provider = ?1 AND gap.trajectory_id IS NOT NULL
                 UNION ALL SELECT session_id FROM native_catalog_homes WHERE provider=?1 AND NOT verified"
            )
        } else {
            format!(
                "SELECT trajectories.session_id FROM visible_trajectories AS trajectories
                 INNER JOIN scoped_session_index AS catalog ON {CONFLICTING_FILE_CATALOG}
                 WHERE trajectories.provider = ?1"
            )
        };
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
        let mut sql = format!("SELECT trajectories.provider, trajectories.session_id, {SEARCH_ACTIVE_TITLE},
            coalesce(active_projection.document_version, trajectories.document_version),
            coalesce(active_projection.source_updated_at, trajectories.source_updated_at), source_updated_submillis, coalesce(title_authoritative, source_complete),
            CASE WHEN trajectories.provider = 'omp' AND coalesce(trajectories.title_authoritative, trajectories.source_complete)
             THEN (SELECT coalesce(catalog.source_updated_submillis, 0)
                   FROM scoped_session_index AS catalog
                   WHERE catalog.provider = trajectories.provider AND catalog.session_id = trajectories.session_id
                     AND catalog.updated_at = trajectories.source_updated_at AND
                     (catalog.source_updated_submillis = trajectories.source_updated_submillis
                      OR (catalog.source_updated_submillis IS NULL AND trajectories.source_complete))) END,
            trajectories.source_file_id, trajectories.missing_ancestor_id, {SEARCH_ACTIVE_CONTENTS}
            FROM visible_trajectories AS trajectories {SEARCH_GAP_JOIN}
            WHERE NOT EXISTS (SELECT 1 FROM observed_session_index AS catalog WHERE ({CONFLICTING_FILE_CATALOG}) OR ({CONFLICTING_CODEX_CATALOG}))
              AND NOT EXISTS (SELECT 1 FROM scoped_trajectory_failures AS failed WHERE ({HIDDEN_FAILED_TRAJECTORY}) OR ({CONFLICTING_CODEX_FAILURE})) ");
        let scope = if let Some(session) = session {
            validate_session_ref(session)?;
            sql.push_str("AND provider = ?1 AND session_id = ?2");
            vec![session.provider.to_string(), session.id.clone()]
        } else {
            sql.push_str(
                "AND (active_projection.trajectory_id IS NOT NULL OR trajectories.derived_title IS NOT NULL OR (provider = 'omp' AND title_authoritative = 1))",
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
                    document_version: row.get(3).map_err(database_error)?,
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
                    codex_home TEXT,
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
        initialize_index_schemas(&transaction, stored_format)?;
        transaction.commit().map_err(database_error)?;
        if stored_format < STORE_FORMAT {
            codex_document_home::initialize(&mut connection, initialize_provider_receipt_schemas)?;
            // Deleting promoted gaps cascades to their chunks after the format swap restores foreign keys.
            let transaction = immediate_transaction(&mut connection)?;
            database_receipts::promote_gap_projections(&transaction)?;
            transaction.commit().map_err(database_error)?;
        }
        Ok(())
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

    /// Reads cached metadata and title revisions from the same database snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the cached rows cannot be read.
    pub fn cached_sessions_with_titles(
        &self,
        provider: Option<Provider>,
    ) -> Result<(Vec<IndexedSession>, Vec<IndexedTrajectoryTitle>)> {
        let connection = self.connection.borrow();
        let _snapshot = connection.unchecked_transaction().map_err(database_error)?;
        let filter = if provider.is_some() {
            "AND catalog.provider=?1"
        } else {
            ""
        };
        // Older writers can refresh the catalog without recording the selected file. When
        // cached search borrows its document, use the workspace established for that document.
        let sql = format!(
            "SELECT catalog.provider,catalog.session_id,catalog.title,
               CASE WHEN pending.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NOT NULL
                    THEN w.project_path ELSE catalog.project_path END,
               CASE WHEN pending.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NOT NULL
                    THEN w.git_branch ELSE catalog.git_branch END,
               catalog.created_at,catalog.updated_at,catalog.updated_at_approximate,catalog.event_count,catalog.source_updated_submillis,
               catalog.source_file_id,catalog.source_contents_id
             FROM observed_session_index catalog LEFT JOIN visible_trajectories trajectories ON catalog.provider='codex'
               AND catalog.source_file_id IS NULL AND catalog.source_contents_id IS NULL
               AND trajectories.provider=catalog.provider AND trajectories.session_id=catalog.session_id
               AND trajectories.origin='native'
             LEFT JOIN scoped_trajectory_failures failed ON {CONFLICTING_CODEX_FAILURE}
             LEFT JOIN codex_indexed_workspaces w ON failed.session_id IS NULL AND w.trajectory_id=trajectories.id
               AND w.source_file_id=trajectories.source_file_id AND w.source_contents_id=trajectories.source_contents_id
             LEFT JOIN codex_workspace_rechecks pending ON failed.session_id IS NULL AND pending.trajectory_id=trajectories.id
               AND pending.source_file_id=trajectories.source_file_id AND pending.source_contents_id=trajectories.source_contents_id
             WHERE {VISIBLE_CATALOG_ROW} {filter}
             ORDER BY catalog.updated_at IS NULL,catalog.updated_at DESC,catalog.provider,catalog.session_id"
        );
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let sessions = statement
            .query_map(
                rusqlite::params_from_iter(provider.map(|p| p.to_string())),
                indexed_session_from_row,
            )
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(|_| StoreError::CorruptStore)?;
        Ok((sessions, self.trajectory_titles_with_sources()?))
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
            if catalog_session(&transaction, &prior.session, true)?.as_ref() != Some(prior) {
                continue;
            }
            let revision = prior.source_revision.unwrap_or_default();
            transaction.execute(
                "DELETE FROM codex_workspace_observations WHERE provider_home=(SELECT home FROM current_codex_home) AND session_id=?1
                 AND (source_file_id,source_contents_id,source_updated_at) IS (?2,?3,?4)",
                params![prior.session.id, revision.file.map(Uuid::into_bytes), revision.contents.map(Uuid::into_bytes),
                        prior.updated_at.map(|t| t.timestamp_millis())]
            ).map_err(database_error)?;
            transaction
                .execute(
                    "DELETE FROM session_trajectories WHERE provider='codex' AND session_id=?1 AND codex_home=(SELECT home FROM current_codex_home)
                 AND origin='native' AND protected_by_bundle=0 AND source_file_id IS ?2
                 AND source_contents_id IS ?3 AND source_updated_at IS ?4",
                    params![
                        prior.session.id,
                        revision.file.map(Uuid::into_bytes),
                        revision.contents.map(Uuid::into_bytes),
                        prior.updated_at.map(|t| t.timestamp_millis())
                    ],
                )
                .map_err(database_error)?;
            transaction
                .execute(
                    "DELETE FROM session_index WHERE provider='codex' AND session_id=?1 AND codex_home=(SELECT home FROM current_codex_home)",
                    [&prior.session.id],
                )
                .map_err(database_error)?;
        }
        transaction.commit().map_err(database_error)
    }

    /// Reads cached metadata for one selected session.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata cannot be read.
    pub fn indexed_session(&self, session: &SessionRef) -> Result<Option<IndexedSession>> {
        cached_session(&self.connection.borrow(), session)
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
        let observation = SelectedSessionObservation {
            state: cached_trajectory_state(&transaction, session)?,
            metadata: cached_session(&transaction, session)?,
            failure: index_failure(&transaction, session)?,
            content_hash: cached_content_hash(&transaction, session)?,
        };
        transaction.commit().map_err(database_error)?;
        Ok(observation)
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
        self.publish_workspace_documents(
            documents.iter().copied(),
            MAX_TRAJECTORY_WRITE_BATCH_DOCUMENTS,
            if documents.len() == 1 {
                usize::MAX
            } else {
                MAX_TRAJECTORY_WRITE_BATCH_BYTES
            },
            None,
        )
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
        if !selected
            .source_revision
            .is_some_and(|revision| revision.matches_file_contents(document.source_revision))
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let cleared;
        let selected = if matches!(workspace, Some(CodexWorkspaceUpdate::Withdrawn(_))) {
            cleared = IndexedSession {
                project_path: None,
                git_branch: None,
                ..selected.clone()
            };
            &cleared
        } else {
            selected
        };
        self.publish_workspace_documents(
            std::iter::once((
                &selected.session,
                document,
                TrajectorySourceGuard::ObservedRevision(observation.state),
                workspace,
            )),
            1,
            usize::MAX,
            Some(SelectedMetadata {
                selected,
                observation,
            }),
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
            || cached_session(&transaction, &selected.session)?.as_ref() != observed_metadata
            || index_failure(&transaction, &selected.session)? != failure
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        write_indexed_session(&transaction, selected)?;
        if let Some(failure) = failure {
            clear_observed_failure(&transaction, &selected.session, failure)?;
        }
        transaction.commit().map_err(database_error)
    }

    fn publish_workspace_documents<'s, 'd>(
        &self,
        documents: impl ExactSizeIterator<
            Item = (
                &'s SessionRef,
                TrajectoryDocument<'d>,
                TrajectorySourceGuard,
                Option<&'d CodexWorkspaceUpdate>,
            ),
        >,
        max_documents: usize,
        max_bytes: usize,
        selected_metadata: Option<SelectedMetadata<'_>>,
    ) -> Result<()> {
        if documents.len() > max_documents {
            return Err(StoreError::TrajectoryBatchTooLarge);
        }
        let mut remaining = max_bytes;
        let mut prepared = Vec::with_capacity(documents.len());
        for (session, document, guard, workspace) in documents {
            let mut write =
                PreparedTrajectoryWrite::new((session, document, guard, None), remaining)?;
            remaining -= document.indexed_byte_count;
            write.workspace = workspace;
            prepared.push(write);
        }
        self.publish_trajectory_documents(
            &prepared,
            remaining,
            &HashMap::new(),
            |_, _| true,
            selected_metadata,
        )
        .map(|_| ())
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
                    SELECT 1 FROM visible_trajectories
                    WHERE provider = ?1 AND session_id = ?2
                      AND source_complete = 1 AND source_updated_at >= ?3
                      AND (NOT ?4 OR (source_file_id IS ?5 AND missing_ancestor_id IS ?6 AND source_contents_id IS ?8))
                      AND (NOT ?7 OR source_updated_at = ?3) AND document_version >= ?9
                )",
                params![
                    session.provider.to_string(),
                    session.id,
                    source_updated_at.timestamp_millis(),
                    source_revision.is_some(),
                    revision.file.map(Uuid::into_bytes),
                    revision.missing_ancestor.map(Uuid::into_bytes),
                    session.provider == Provider::Codex,
                    revision.contents.map(Uuid::into_bytes),
                    document_version,
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)
    }

    /// Reads the digest of a Codex projection that can be verified against the selected rollout.
    /// A partial projection must still refer to the same physical file and contents.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database read.
    pub fn codex_projection_hash(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
        revision: SourceRevision,
    ) -> Result<Option<[u8; 32]>> {
        validate_session_ref(session)?;
        if session.provider != Provider::Codex {
            return Ok(None);
        }
        self.connection
            .borrow()
            .query_row(
                "SELECT t.content_hash FROM visible_trajectories AS t
                 WHERE t.provider = 'codex' AND t.session_id = ?1 AND t.source_updated_at = ?2
                   AND t.origin = 'native' AND typeof(t.content_hash) = 'blob' AND length(t.content_hash) = 32
                   AND t.source_file_id IS ?3 AND t.source_contents_id IS ?4 AND t.missing_ancestor_id IS NULL
                   AND ((?3 IS NOT NULL AND ?4 IS NOT NULL AND t.source_complete = 0)
                     OR (?3 IS NULL AND ?4 IS NULL AND t.source_complete = 1
                       AND (t.document_version = 4 OR (t.document_version = 5 AND EXISTS (
                           SELECT 1 FROM legacy_codex_v5_projections AS old
                           WHERE old.trajectory_id = t.id AND old.source_updated_at = t.source_updated_at
                             AND old.content_hash = t.content_hash)))))",
                params![session.id, source_updated_at.timestamp_millis(), revision.file.map(Uuid::into_bytes),
                        revision.contents.map(Uuid::into_bytes)],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)
    }

    /// Reads the digest of a full Codex projection that predates file and contents revisions.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid session reference or a failed database read.
    pub fn legacy_codex_projection_hash(
        &self,
        session: &SessionRef,
        source_updated_at: DateTime<Utc>,
    ) -> Result<Option<[u8; 32]>> {
        self.codex_projection_hash(session, source_updated_at, SourceRevision::default())
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
            "SELECT derived_title FROM visible_trajectories
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
        let connection = self.connection.borrow();
        let sql = format!(
            "SELECT trajectories.session_id FROM visible_trajectories AS trajectories
                          INNER JOIN observed_session_index AS catalog ON {CONFLICTING_CODEX_CATALOG}"
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
            .borrow()
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
        let connection = self.connection.borrow();
        let sql = format!(
            "{} WHERE trajectories.provider = 'codex' OR active_projection.trajectory_id IS NOT NULL OR length(trajectories.derived_title) > 0 OR (trajectories.provider='omp' AND trajectories.title_authoritative)",
            indexed_titles_sql()
        );
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        statement
            .query_map([], indexed_title_from_row)
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
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
             SELECT provider, session_id, NULL, NULL FROM visible_trajectories
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

const SEARCH_GAP_JOIN: &str = "LEFT JOIN antigravity_search_gaps gap
    ON gap.trajectory_id=trajectories.id AND gap.source_file=trajectories.source_file_id
    AND gap.indexed_contents=trajectories.source_contents_id AND gap.indexed_hash=trajectories.content_hash
    AND EXISTS (SELECT 1 FROM native_search_document_homes h WHERE h.trajectory_id=trajectories.id
                AND h.verification=1 AND h.source_file=gap.source_file AND h.source_home=gap.source_home)
    LEFT JOIN antigravity_gap_projections active_projection ON active_projection.trajectory_id=gap.trajectory_id";

const SEARCH_COVERAGE_COLUMNS: &str = "trajectories.source_complete AND gap.trajectory_id IS NULL,
    trajectories.complete AND gap.trajectory_id IS NULL,
    CASE WHEN gap.trajectory_id IS NULL THEN trajectories.indexed_byte_count ELSE coalesce(active_projection.indexed_byte_count,trajectories.indexed_byte_count) END,
    CASE WHEN gap.trajectory_id IS NULL THEN trajectories.source_byte_count ELSE coalesce(active_projection.source_byte_count,trajectories.source_byte_count) END,
    CASE WHEN gap.trajectory_id IS NULL THEN trajectories.truncation_strategy ELSE 'source_incomplete' END";

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
                {SEARCH_COVERAGE_COLUMNS}, ranked_chunks.chunk_id, 0
         FROM ranked_chunks
         INNER JOIN session_trajectories AS trajectories
             ON trajectories.id = ranked_chunks.trajectory_id
         {SEARCH_GAP_JOIN}
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
                {SEARCH_COVERAGE_COLUMNS},
                best_snippets.chunk_id, best_snippets.clause_index
         FROM ranked_trajectories
         INNER JOIN best_snippets
             ON best_snippets.trajectory_id = ranked_trajectories.trajectory_id
            AND best_snippets.snippet_rank = 1
         INNER JOIN session_trajectories AS trajectories
             ON trajectories.id = ranked_trajectories.trajectory_id
         {SEARCH_GAP_JOIN}
         ORDER BY ranked_trajectories.trajectory_rank,
                  trajectories.source_updated_at DESC,
                  trajectories.provider, trajectories.session_id"
    )
    .expect("writing SQL into a string cannot fail");
    sql
}

fn initialize_trajectory_scope(connection: &Connection, codex_home: Option<&str>) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TEMP TABLE current_codex_home (home TEXT);
         CREATE TEMP VIEW visible_trajectories AS SELECT * FROM main.session_trajectories AS t
         WHERE t.provider <> 'codex'
            OR (t.origin = 'native' AND t.codex_workspace_current
                AND t.codex_home = (SELECT home FROM current_codex_home));
         CREATE TEMP VIEW scoped_session_index AS SELECT * FROM main.session_index
         WHERE provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home);
         CREATE TEMP VIEW scoped_trajectory_failures AS SELECT * FROM main.trajectory_index_failures
         WHERE provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home);
         CREATE TEMP VIEW observed_session_index AS
         WITH observed AS (
             SELECT 'codex' AS provider, w.session_id,
                    CASE WHEN i.session_id IS NULL THEN w.title ELSE i.title END AS title,
                    w.project_path, w.git_branch,
                    CASE WHEN i.session_id IS NULL THEN w.created_at ELSE i.created_at END AS created_at,
                    w.source_updated_at AS updated_at, 0 AS updated_at_approximate,
                    coalesce(i.event_count,w.event_count) AS event_count, i.source_updated_submillis,
                    w.source_file_id, w.source_contents_id
             FROM codex_workspace_observations AS w LEFT JOIN scoped_session_index AS i
               ON i.provider = 'codex' AND i.session_id = w.session_id
             WHERE w.provider_home = (SELECT home FROM current_codex_home)
               AND (i.session_id IS NULL OR (i.updated_at = w.source_updated_at AND NOT i.updated_at_approximate
                    AND (w.source_file_id,w.source_contents_id) IS (i.source_file_id,i.source_contents_id)))
         )
         SELECT i.provider,i.session_id,i.title,i.project_path,i.git_branch,i.created_at,
                i.updated_at,i.updated_at_approximate,i.event_count,i.source_updated_submillis,
                i.source_file_id,i.source_contents_id
         FROM scoped_session_index AS i WHERE i.provider <> 'codex' OR NOT EXISTS(
             SELECT 1 FROM observed AS w WHERE w.session_id = i.session_id)
         UNION ALL SELECT * FROM observed;",
        )
        .map_err(database_error)?;
    connection
        .execute("INSERT INTO current_codex_home VALUES (?1)", [codex_home])
        .map_err(database_error)?;
    Ok(())
}

fn active_codex_home() -> Option<String> {
    std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))
        .and_then(|path| codex_home_key(&path))
}

fn codex_home_key(path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).ok()?;
    // A missing provider home can still have cached sessions under a symlinked parent directory.
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
                    source_file_id, source_contents_id FROM scoped_trajectory_failures
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
    check_key: &str,
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
                "DELETE FROM session_index WHERE provider = ?1 AND (provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home))",
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
                params![check_key, indexed_at],
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
    check_key: &str,
) -> Result<bool> {
    let refreshed: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_index_state WHERE provider = ?1)",
            [check_key],
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

fn prune_stale_native_trajectories(
    transaction: &Transaction<'_>,
    provider: &str,
    codex_home: Option<&str>,
) -> Result<()> {
    if provider == "codex" {
        transaction.execute(
            "DELETE FROM codex_workspace_observations AS w WHERE w.provider_home = ?1 AND NOT EXISTS (
                 SELECT 1 FROM session_index WHERE provider = 'codex' AND session_id = w.session_id AND codex_home = ?1)",
            [codex_home],
        ).map_err(database_error)?;
    }
    transaction
        .execute(
            "DELETE FROM session_trajectories
             WHERE provider = ?1
               AND protected_by_bundle = 0
               AND (provider <> 'codex' OR codex_home = ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM session_index
                   WHERE session_index.provider = session_trajectories.provider
                     AND session_index.session_id = session_trajectories.session_id
                     AND (session_index.provider <> 'codex'
                          OR session_index.codex_home = session_trajectories.codex_home)
               )",
            params![provider, codex_home],
        )
        .map_err(database_error)?;
    Ok(())
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
        legacy_file: None,
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

fn initialize_index_schemas(transaction: &Transaction<'_>, stored_format: i64) -> Result<()> {
    initialize_session_index_checks(transaction)?;
    ensure_session_index_columns(transaction)?;
    initialize_codex_workspace_schema(transaction)?;
    initialize_bundle_source_index(transaction)?;
    initialize_trajectory_schema(transaction, stored_format)?;
    ensure_source_time_columns(transaction)?;
    initialize_source_revision_schema(transaction)?;
    if stored_format == STORE_FORMAT {
        initialize_provider_receipt_schemas(transaction)?;
        database_receipts::promote_gap_projections(transaction)?;
    }
    let has_revision: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('codex_workspace_observations') WHERE name='source_file_id')",
        [], |row| row.get(0),
    ).map_err(database_error)?;
    if !has_revision {
        transaction
            .execute_batch(
                "ALTER TABLE codex_workspace_observations ADD COLUMN source_file_id BLOB;
             ALTER TABLE codex_workspace_observations ADD COLUMN source_contents_id BLOB;",
            )
            .map_err(database_error)?;
        if stored_format == STORE_FORMAT {
            initialize_codex_document_triggers(transaction)?;
        }
    }
    Ok(())
}

fn initialize_provider_receipt_schemas(transaction: &Transaction<'_>) -> Result<()> {
    database_receipts::initialize(transaction)?;
    initialize_discovery_checks(transaction)
}

fn initialize_codex_workspace_schema(transaction: &Transaction<'_>) -> Result<()> {
    for table in ["session_index", "trajectory_index_failures"] {
        let has_home = transaction
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = 'codex_home')",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)?;
        if !has_home {
            transaction
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN codex_home TEXT;"))
                .map_err(database_error)?;
        }
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS codex_workspace_observations (
             provider_home TEXT NOT NULL, session_id TEXT NOT NULL, source_path TEXT NOT NULL,
             source_stamp TEXT NOT NULL, source_updated_at INTEGER NOT NULL, project_path TEXT,
             git_branch TEXT, title TEXT, created_at INTEGER, event_count INTEGER NOT NULL,
             PRIMARY KEY (provider_home, session_id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS codex_workspace_session_id ON codex_workspace_observations (session_id);",
        )
        .map_err(database_error)
}

fn remove_codex_workspace(
    transaction: &Transaction<'_>,
    session: &SessionRef,
    home: Option<&str>,
) -> Result<()> {
    if session.provider == Provider::Codex {
        transaction
            .execute(
                "DELETE FROM codex_workspace_observations WHERE session_id = ?1 AND provider_home = ?2",
                params![session.id, home],
            )
            .map_err(database_error)?;
    }
    Ok(())
}

fn initialize_trajectory_schema(transaction: &Transaction<'_>, stored_format: i64) -> Result<()> {
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
    initialize_legacy_codex_v5_projections(transaction)?;
    ensure_trajectory_source_revision_columns(transaction)?;
    codex_workspace::initialize(transaction)?;
    ensure_trajectory_title_provenance(transaction)?;
    codex_title_recheck::schedule_tool_catalog_title_rechecks(transaction)?;
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
    initialize_trajectory_chunk_schema(transaction)?;
    if stored_format == 0 {
        transaction
            .pragma_update(None, "user_version", 1)
            .map_err(database_error)?;
    }
    Ok(())
}

fn initialize_codex_document_triggers(transaction: &Transaction<'_>) -> Result<()> {
    // The selected document or failure can move to another rollout before an old observation expires.
    let observed_source = "(source_file_id IS NULL OR
        (source_file_id = old.source_file_id
         AND (source_contents_id IS NULL OR source_contents_id = old.source_contents_id)))";
    for (name, event) in [
        (
            "changed",
            "UPDATE OF source_stamp,source_file_id,source_contents_id ON codex_workspace_observations
             WHEN old.source_stamp IS NOT new.source_stamp
               OR (old.source_file_id IS NOT NULL AND old.source_file_id IS NOT new.source_file_id)
               OR (old.source_contents_id IS NOT NULL AND old.source_contents_id IS NOT new.source_contents_id)",
        ),
        ("removed", "DELETE ON codex_workspace_observations"),
    ] {
        transaction.execute_batch(&format!(
            "DROP TRIGGER IF EXISTS codex_workspace_{name};
             CREATE TRIGGER codex_workspace_{name} AFTER {event} BEGIN
                 UPDATE session_trajectories SET codex_workspace_current = 0
                  WHERE provider = 'codex' AND origin = 'native'
                    AND session_id = old.session_id AND codex_home = old.provider_home AND {observed_source};
                 DELETE FROM trajectory_index_failures
                  WHERE provider = 'codex' AND session_id = old.session_id AND codex_home = old.provider_home
                    AND {observed_source};
             END;"
        )).map_err(database_error)?;
    }
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
    let has_column = |name: &str| columns.iter().any(|column| column == name);
    if !has_column("projected_byte_count") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN projected_byte_count INTEGER CHECK (projected_byte_count >= 0)").map_err(database_error)?;
    }
    if !has_column("projected_prefix_hash") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN projected_prefix_hash BLOB CHECK (length(projected_prefix_hash)=32)").map_err(database_error)?;
    }
    if !has_column("verified_record_time") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories ADD COLUMN verified_record_time INTEGER",
            )
            .map_err(database_error)?;
    }
    if !has_column("recovered_anchor_bytes") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories ADD COLUMN recovered_anchor_bytes INTEGER",
            )
            .map_err(database_error)?;
    }
    if !has_column("fully_scanned_bytes") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN fully_scanned_bytes INTEGER CHECK (fully_scanned_bytes >= 0)").map_err(database_error)?;
    }
    if !has_column("sampled_file_bytes") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN sampled_file_bytes INTEGER CHECK (sampled_file_bytes >= 0)").map_err(database_error)?;
    }
    if !has_column("recovered_gap_start") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN recovered_gap_start INTEGER;
            ALTER TABLE session_trajectories ADD COLUMN observed_text_bytes INTEGER;
            ALTER TABLE session_trajectories ADD COLUMN observed_text_hash BLOB CHECK (length(observed_text_hash)=32)").map_err(database_error)?;
    }
    if !has_column("projected_record_prefix_hash") {
        transaction.execute_batch("ALTER TABLE session_trajectories ADD COLUMN projected_record_prefix_hash BLOB CHECK (length(projected_record_prefix_hash)=32)").map_err(database_error)?;
    }
    if !columns.iter().any(|column| column == "document_version") {
        transaction
            .execute_batch(
                "ALTER TABLE session_trajectories
                 ADD COLUMN document_version INTEGER NOT NULL DEFAULT 0
                 CHECK (document_version >= 0);",
            )
            .map_err(database_error)?;
    }
    if !columns.iter().any(|column| column == "codex_home") {
        transaction
            .execute_batch("ALTER TABLE session_trajectories ADD COLUMN codex_home TEXT;")
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
                 origin = 'imported_bundle' OR (provider <> 'codex' AND (provider, session_id) IN (
                     SELECT {BUNDLE_SOURCE_PROVIDER_SQL}, {BUNDLE_SOURCE_SESSION_ID_SQL}
                     FROM bundles
                 ))
             );"
        ))
        .map_err(database_error)
}

fn protect_bundle_trajectory(transaction: &Transaction<'_>, session: &SessionRef) -> Result<()> {
    transaction
        .execute(
            "UPDATE session_trajectories SET protected_by_bundle = 1
             WHERE provider = ?1 AND session_id = ?2
               AND (provider <> 'codex' OR origin = 'imported_bundle'
                    OR codex_home = (SELECT home FROM current_codex_home))",
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

#[derive(Clone, Copy)]
enum ProjectionWindow {
    Uncapped,
    Measured(usize),
    Legacy(usize),
}

impl ProjectionWindow {
    fn for_documents(
        strategy: &str,
        old_projected: Option<i64>,
        previous_bytes: usize,
        document: &TrajectoryDocument<'_>,
    ) -> Self {
        let windowed = strategy.ends_with("document_head_tail")
            && document.truncation_strategy.ends_with("document_head_tail");
        let known_count = |measured: Option<usize>, strategy: &str, raw| {
            measured.or_else(|| (!strategy.contains("event")).then_some(raw))
        };
        let prior_count = known_count(
            old_projected.and_then(|n| usize::try_from(n).ok()),
            strategy,
            previous_bytes,
        );
        let current_count = known_count(
            document.projection.map(|p| p.byte_count),
            document.truncation_strategy,
            document.source_byte_count,
        );
        if !windowed {
            ProjectionWindow::Uncapped
        } else if let Some((old, new)) = prior_count.zip(current_count) {
            ProjectionWindow::Measured(old.saturating_sub(new))
        } else {
            ProjectionWindow::Legacy(previous_bytes - document.source_byte_count)
        }
    }
}

// Hash each confirmed byte once while testing the record boundaries in encounter order.
fn verified_gap_start(text: &str, starts: &[usize], digest: &[u8]) -> Option<usize> {
    let mut hash = Sha256::new();
    let mut previous = 0;
    for &start in starts {
        hash.update(text.get(previous..start)?.as_bytes());
        if hash.clone().finalize().as_slice() == digest {
            return Some(start);
        }
        previous = start;
    }
    None
}

// An R19 preview kept a contiguous head and tail. Match both at a confirmed gap
// before carrying its verified final record into the full projection.
fn legacy_record_at_gap(
    previous: &str,
    digest: &[u8],
    observed: &str,
    starts: &[usize],
) -> Option<RecoveredRecord> {
    let mut hash = Sha256::new();
    let mut consumed = 0;
    let boundary = previous.match_indices("\n\n").find_map(|(start, _)| {
        hash.update(previous.get(consumed..start)?.as_bytes());
        consumed = start;
        (hash.clone().finalize().as_slice() == digest).then_some(start)
    })?;
    let (sample, gap) = previous.split_at(boundary);
    let shared = sample
        .split("\n\n")
        .zip(observed.split("\n\n"))
        .take_while(|(old, new)| old == new)
        .map(|(word, _)| word.len() + 2)
        .sum::<usize>();
    let tail = sample.get(shared.saturating_sub(2)..)?;
    let start = if tail.is_empty() {
        starts.binary_search(&boundary).ok().map(|_| boundary)?
    } else {
        if shared == 0 {
            return None;
        }
        let mut matches = observed.match_indices(tail).filter_map(|(offset, _)| {
            let end = offset + tail.len();
            (offset >= shared && starts.binary_search(&end).is_ok()).then_some(end)
        });
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        first
    };
    let (before, after) = observed.split_at(start);
    if observed.len() + gap.len() > 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT
        || record_occurrences(after, gap.strip_prefix("\n\n")?)
            .next()
            .is_some()
    {
        return None;
    }
    Some(RecoveredRecord {
        text: [before, gap, after].concat(),
        start,
        evidence: RecordEvidence {
            time: None,
            anchor: (!after.is_empty()).then_some(after.len()),
        },
    })
}

struct RecordEvidence {
    time: Option<i64>,
    anchor: Option<usize>,
}

enum LegacySampleUpdate {
    Keep,
    Observed,
    Restore(RecoveredRecord),
}

struct RecoveredRecord {
    text: String,
    start: usize,
    evidence: RecordEvidence,
}

fn recover_missing_record_text(
    observed: &str,
    previous: &str,
    start: usize,
    missing: usize,
    previously_recovered: bool,
    mut evidence: RecordEvidence,
    dated_records: &[(usize, usize, i64)],
) -> Option<RecoveredRecord> {
    let separator = if start == 0 && !observed.is_empty() && !previously_recovered {
        "\n\n"
    } else {
        ""
    };
    let size = observed
        .len()
        .checked_add(missing)?
        .checked_add(separator.len())?;
    if missing == 0 || size > 2 * omnis_ir::SEARCH_DOCUMENT_EDGE_BYTE_LIMIT {
        return None;
    }
    let (prefix, suffix) = observed.split_at_checked(start)?;
    let gap = previous.strip_prefix(prefix)?.get(..missing)?;
    let record = if start > 0 {
        gap.strip_prefix("\n\n")?
    } else if previously_recovered {
        gap.strip_suffix("\n\n")?
    } else {
        gap
    };
    let prior_suffix = previous.get(start.checked_add(missing)?..)?;
    let anchor = evidence
        .anchor
        .or_else(|| {
            (previously_recovered && record_occurrences(prior_suffix, record).next().is_none())
                .then_some(prior_suffix.len())
        })
        .filter(|&len| {
            len > 0
                && prior_suffix
                    .get(..len)
                    .and_then(|anchor| suffix.strip_prefix(anchor))
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with("\n\n"))
        });
    let mut matches = record_occurrences(suffix, record).peekable();
    let repeated = matches.peek().is_some();
    if repeated
        && anchor.is_none()
        && !matches.all(|offset| {
            let position = start + offset;
            evidence.time.is_some_and(|old| {
                occurrence_is_later(dated_records, position, position + record.len(), old)
            })
        })
    {
        return None;
    }
    let mut text = String::with_capacity(size);
    text.push_str(prefix);
    text.push_str(gap);
    text.push_str(separator);
    text.push_str(suffix);
    evidence.anchor = anchor.or_else(|| (!repeated && !suffix.is_empty()).then_some(suffix.len()));
    Some(RecoveredRecord {
        text,
        start,
        evidence,
    })
}

fn occurrence_is_later(
    dated_records: &[(usize, usize, i64)],
    start: usize,
    end: usize,
    historical_time: i64,
) -> bool {
    let first = dated_records.partition_point(|&(_, to, _)| to <= start);
    let mut position = start;
    for &(from, to, time) in &dated_records[first..] {
        if from > position || time <= historical_time {
            return false;
        }
        if to >= end {
            return true;
        }
        position = to + "\n\n".len();
    }
    false
}

fn record_occurrences<'a>(suffix: &'a str, record: &'a str) -> impl Iterator<Item = usize> + 'a {
    suffix
        .match_indices(record)
        .filter_map(move |(start, matched)| {
            let end = start + matched.len();
            (!record.is_empty()
                && (start == 0 || suffix[..start].ends_with("\n\n"))
                && (end == suffix.len() || suffix[end..].starts_with("\n\n")))
            .then_some(start)
        })
}

impl RecoveredRecord {
    fn store(&self, transaction: &Transaction<'_>, id: i64) -> Result<()> {
        transaction.execute(
            "UPDATE session_trajectories SET recovered_gap_start=?2, observed_text_bytes=source_byte_count,
             observed_text_hash=content_hash, indexed_byte_count=?3, source_byte_count=?3, content_hash=?4,
             verified_record_time=?5, recovered_anchor_bytes=?6 WHERE id=?1",
            params![id, i64::try_from(self.start).expect("bounded text"), i64::try_from(self.text.len()).expect("bounded text"), Sha256::digest(self.text.as_bytes()).as_slice(), self.evidence.time, self.evidence.anchor.map(|n| i64::try_from(n).expect("bounded text"))]
        ).map_err(database_error)?;
        Ok(())
    }
}

fn stored_projection_text(
    transaction: &Transaction<'_>,
    id: i64,
    length: usize,
) -> Result<Option<String>> {
    let mut statement = transaction.prepare_cached(
        "SELECT stored_text FROM session_trajectory_chunk_rows WHERE trajectory_id=?1 AND NOT EXISTS (SELECT 1 FROM antigravity_gap_projections p WHERE p.trajectory_id=?1 AND chunk_index>=p.first_chunk) ORDER BY chunk_index",
    ).map_err(database_error)?;
    let mut rows = statement.query([id]).map_err(database_error)?;
    let mut previous = String::with_capacity(length);
    let mut scratch = Vec::new();
    while let Some(row) = rows.next().map_err(database_error)? {
        let Some(chunk) = chunk_text(row.get_ref(0).map_err(database_error)?, &mut scratch) else {
            return Ok(None);
        };
        let overlap = chunk_overlap_start(&previous);
        let Some(rest) = chunk.strip_prefix(&previous[overlap..]) else {
            return Ok(None);
        };
        if previous.len().saturating_add(rest.len()) > length {
            return Ok(None);
        }
        previous.push_str(rest);
    }
    if previous.len() != length {
        return Ok(None);
    }
    Ok(Some(previous))
}

// Match complete paragraphs in order. An omission may fall between two retained events.
fn stored_text_contains_projection(
    transaction: &Transaction<'_>,
    id: i64,
    length: usize,
    projection: &str,
    window: ProjectionWindow,
    legacy_prefix: Option<(&omnis_ir::SearchProjection, &[u8])>,
) -> Result<bool> {
    let Some(previous) = stored_projection_text(transaction, id, length)? else {
        return Ok(false);
    };
    if !matches!(window, ProjectionWindow::Uncapped)
        && let (Some((old_head, old_tail)), Some((head, tail))) = (
            omnis_ir::search_document_windows(&previous),
            omnis_ir::search_document_windows(projection),
        )
    {
        if let Some((proof, digest)) = legacy_prefix {
            // R18 hashed through the penultimate event, including any earlier events of
            // the omitted planner record. R19 hashed only records before that record.
            let ProjectionWindow::Measured(delta) = window else {
                return Ok(false);
            };
            let suffix = old_tail
                .len()
                .checked_sub(delta)
                .and_then(|start| old_tail.get(start..));
            return Ok(suffix.is_some_and(|rest| {
                rest.starts_with("\n\n")
                    && rest.rsplit_once("\n\n").is_some_and(|(before_last, _)| {
                        proof.matches_with_suffix(before_last.as_bytes(), digest)
                    })
            }));
        }
        let matches = |old: &str, current: &str, tail: bool| {
            let delta = match window {
                ProjectionWindow::Measured(n) => {
                    // A removed suffix begins at an event separator. Compare every byte
                    // that this window and the old document share.
                    if tail
                        && let Some(start) = old.len().checked_sub(n)
                        && start > 0
                        && old
                            .get(start..)
                            .is_some_and(|rest| rest.starts_with("\n\n"))
                        && current.as_bytes().ends_with(&old.as_bytes()[..start])
                    {
                        return true;
                    }
                    Some(n)
                }
                // Without a stored projection length, a complete capped message at the edge
                // can establish the number of retained bytes that disappeared.
                ProjectionWindow::Legacy(raw_delta) => {
                    if tail && matches_planner_tail(old, current, raw_delta) {
                        return true;
                    }
                    let Some(size) = omnis_ir::capped_search_event_at_edge(old, tail)
                        .filter(|&n| n <= raw_delta)
                    else {
                        return false;
                    };
                    let old = old.as_bytes();
                    // The newly exposed inside edge was outside the old window. Compare all
                    // previously retained bytes except the omitted event at the outside edge.
                    return if tail {
                        current.as_bytes().ends_with(&old[..old.len() - size])
                    } else {
                        current.as_bytes().starts_with(&old[size..])
                    };
                }
                ProjectionWindow::Uncapped => None,
            };
            delta.is_some_and(|n| {
                n > 0 && matches_window_after_omission(old, current, n, tail, &previous)
            })
        };
        if (head == old_head && matches(old_tail, tail, true))
            || (tail == old_tail && matches(old_head, head, false))
        {
            return Ok(true);
        }
    }
    let mut paragraphs = previous.split("\n\n");
    Ok(projection
        .split("\n\n")
        .all(|part| paragraphs.any(|old| old == part)))
}

fn matches_planner_tail(old: &str, current: &str, raw_delta: usize) -> bool {
    let mut remaining = old;
    while let Some((prefix, tool)) = remaining.rsplit_once("\n\n") {
        let capped = omnis_ir::capped_search_event_at_edge(remaining, true);
        let tool_like = tool.starts_with("{\"arguments\":") || tool.starts_with("{\"name\":");
        if !tool_like || tool.len() > 2 * omnis_ir::SEARCH_DOCUMENT_TOOL_EDGE_BYTE_LIMIT {
            return false;
        }
        if capped.is_none()
            && !serde_json::from_str::<serde_json::Value>(tool).is_ok_and(|v| {
                v.get("name").is_some_and(serde_json::Value::is_string)
                    && v.get("arguments").is_some()
            })
        {
            return false;
        }
        remaining = prefix;
        if let Some(size) = omnis_ir::capped_search_event_at_edge(remaining, true) {
            let removed = old.len() - remaining.len() + size;
            if removed <= raw_delta
                && current
                    .as_bytes()
                    .ends_with(&old.as_bytes()[..old.len() - removed])
            {
                return true;
            }
        }
    }
    false
}

fn matches_window_after_omission(
    old: &str,
    current: &str,
    delta: usize,
    tail: bool,
    previous: &str,
) -> bool {
    let Some(retained) = old.len().checked_sub(delta) else {
        return false;
    };
    if retained == 0 || current.len() < retained {
        return false;
    }
    // Shrinking the document exposes bytes at the inside edge of this window.
    // Keep the older result only if it already contains those bytes.
    let current = current.as_bytes();
    let (overlap, fringe) = if tail {
        let (fringe, overlap) = current.split_at(current.len() - retained);
        (overlap, fringe)
    } else {
        current.split_at(retained)
    };
    if memchr::memmem::find(previous.as_bytes(), fringe).is_none() {
        return false;
    }
    let old = old.as_bytes();
    let common_prefix = old.iter().zip(overlap).take_while(|(a, b)| a == b).count();
    old[common_prefix + delta..] == overlap[common_prefix..]
}

fn chunk_overlap_start(chunk: &str) -> usize {
    chunk
        .char_indices()
        .rev()
        .nth(SEARCH_QUERY_MAX_CHARS - 1)
        .map_or(
            chunk.len(),
            |(index, _)| if index == 0 { chunk.len() } else { index },
        )
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
            offset += chunk_overlap_start(chunk);
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

fn indexed_workspace(
    row: &rusqlite::Row<'_>,
    project_column: usize,
    provider: Provider,
) -> rusqlite::Result<(Option<PathBuf>, Option<String>)> {
    let path = row
        .get::<_, Option<String>>(project_column)?
        .map(PathBuf::from);
    if provider == Provider::Codex && path.as_deref().is_none_or(Path::is_relative) {
        return Ok((None, None));
    }
    Ok((path, row.get(project_column + 1)?))
}

fn codex_workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedCodexWorkspace> {
    let (project_path, git_branch) = indexed_workspace(row, 2, Provider::Codex)?;
    Ok(IndexedCodexWorkspace {
        source_path: PathBuf::from(row.get::<_, String>(0)?),
        source_stamp: row.get(1)?,
        project_path,
        git_branch,
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
            source_file_id, source_contents_id, codex_home
          ) SELECT ?1,?2,?3,CASE WHEN p.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NULL THEN ?4 ELSE w.project_path END,
                 CASE WHEN p.trajectory_id IS NOT NULL THEN NULL WHEN w.trajectory_id IS NULL THEN ?5 ELSE w.git_branch END,?6,?7,?8,?9,?10,?11,?12,?13,CASE WHEN ?1='codex' THEN (SELECT home FROM current_codex_home) END
          FROM (SELECT 1) LEFT JOIN codex_indexed_workspaces w
            ON ?1='codex' AND w.source_file_id=?12 AND w.source_contents_id IS ?13
            AND w.trajectory_id=(SELECT id FROM visible_trajectories WHERE provider=?1 AND session_id=?2)
          LEFT JOIN codex_workspace_rechecks p ON ?1='codex' AND p.source_file_id=?12 AND p.source_contents_id=?13
            AND p.trajectory_id=(SELECT id FROM visible_trajectories WHERE provider=?1 AND session_id=?2) WHERE 1
         ON CONFLICT DO UPDATE SET
            title = excluded.title, project_path = excluded.project_path,
            git_branch = excluded.git_branch, created_at = excluded.created_at,
            updated_at = excluded.updated_at, updated_at_approximate = excluded.updated_at_approximate,
            event_count = excluded.event_count, indexed_at = excluded.indexed_at,
            source_updated_submillis = excluded.source_updated_submillis,
            source_file_id = excluded.source_file_id, source_contents_id = excluded.source_contents_id
         WHERE ?14 OR excluded.provider NOT IN ('pi','omp') OR (
            (excluded.source_file_id IS NOT NULL OR session_index.source_file_id IS NULL)
            AND (session_index.source_file_id IS NOT NULL OR NOT EXISTS (
                SELECT 1 FROM visible_trajectories AS doc
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
        source_byte_count: usize::try_from(row.get::<_, i64>(column + 9)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(column + 9, Type::Integer, Box::new(error))
        })?,
        legacy_tail_receipt: if samples_jsonl_tail(provider) {
            row.get::<_, Option<String>>(column + 10)?
                .map(|receipt| Sha256::digest(receipt).into())
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
    let (project_path, git_branch) = indexed_workspace(row, 3, provider)?;
    let file = optional_uuid(row, 10)?;
    let contents = optional_uuid(row, 11)?;
    Ok(IndexedSession {
        session: SessionRef::new(provider, row.get::<_, String>(1)?),
        source_revision: file.map(|file| SourceRevision {
            legacy_file: None,
            file: Some(file),
            contents,
            missing_ancestor: None,
        }),
        title: row.get(2)?,
        project_path,
        git_branch,
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
            | "provider_read_limit"
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
        "legacy_bounded" | "legacy_unknown" | "provider_read_limit" => true,
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

mod codex_title_recheck;

use codex_title_recheck::schedule_legacy_codex_title_proofs;

mod codex_workspace;

pub use codex_workspace::{CodexWorkspace, CodexWorkspaceIndex, CodexWorkspaceUpdate};

// A failed replacement hides the old native text until that rollout is selected again.
const CONFLICTING_CODEX_FAILURE: &str = "trajectories.provider = 'codex'
    AND trajectories.origin = 'native' AND failed.provider = trajectories.provider
    AND failed.session_id = trajectories.session_id AND failed.source_file_id IS NOT NULL
    AND (failed.source_file_id IS NOT trajectories.source_file_id
        OR failed.source_contents_id IS NOT trajectories.source_contents_id
        OR failed.source_updated_at IS NOT trajectories.source_updated_at)";

// Discovery can select a replacement before its first transcript read succeeds.
const CONFLICTING_CODEX_CATALOG: &str = "trajectories.provider = 'codex'
    AND trajectories.origin = 'native' AND catalog.provider = trajectories.provider
    AND catalog.session_id = trajectories.session_id AND catalog.source_file_id IS NOT NULL
    AND (catalog.source_file_id IS NOT trajectories.source_file_id
        OR catalog.source_contents_id IS NOT trajectories.source_contents_id)";

#[derive(Clone, Copy)]
struct SelectedMetadata<'a> {
    selected: &'a IndexedSession,
    observation: &'a SelectedSessionObservation,
}

impl SelectedMetadata<'_> {
    fn publish(self, transaction: &Transaction<'_>) -> Result<()> {
        if self.observation.metadata.as_ref() != Some(self.selected) {
            write_indexed_session(transaction, self.selected)?;
        }
        if let Some(failure) = self.observation.failure {
            clear_observed_failure(transaction, &self.selected.session, failure)?;
        }
        Ok(())
    }

    fn check(
        self,
        transaction: &Transaction<'_>,
        session: &SessionRef,
        content_hash: &[u8],
    ) -> Result<()> {
        if index_failure(transaction, session)? != self.observation.failure {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let current = cached_session(transaction, session)?;
        if current != self.observation.metadata && current.as_ref() != Some(self.selected) {
            return Err(StoreError::TrajectorySourceChanged);
        }
        let hash = cached_content_hash(transaction, session)?;
        if hash != self.observation.content_hash && hash.as_deref() != Some(content_hash) {
            return Err(StoreError::TrajectorySourceChanged);
        }
        Ok(())
    }
}

/// The document and catalog state before an explicit selected-source read.
pub struct SelectedSessionObservation {
    pub state: Option<TrajectoryIndexState>,
    pub metadata: Option<IndexedSession>,
    pub failure: Option<TrajectoryIndexFailure>,
    content_hash: Option<Vec<u8>>,
}

/// What replay of the selected ordinary Codex rollout established about its local opener.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LocalOpenerEvidence {
    #[default]
    Unknown,
    /// The first surviving message and any title it supplies are known.
    Current,
    /// The former opener was removed. The surviving replacement could not be read.
    Withdrawn,
    /// Replay verified an ordinary opener against its one-time legacy receipt.
    RecheckedCurrent,
    /// Replay proved the opening message in the one-time legacy receipt was removed.
    RecheckedWithdrawn,
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

/// An indexed title and the physical source that supplied it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedTrajectoryTitle {
    pub document_version: u32,
    pub verified_at: Option<(u32, DateTime<Utc>)>,
    pub cached_at: Option<DateTime<Utc>>,
    pub session: SessionRef,
    pub title: Option<String>,
    pub source_revision: SourceRevision,
}

struct TrajectoryTitleWrite<'a> {
    current: Option<&'a str>,
    withdrawn: Option<&'a str>,
    local: LocalOpenerEvidence,
    verified: bool,
}

impl PreparedTrajectoryWrite<'_, '_> {
    fn publish_codex_parent(
        &self,
        transaction: &Transaction<'_>,
        selected_metadata: Option<SelectedMetadata<'_>>,
    ) -> Result<Option<i64>> {
        let revision = self.document.source_revision;
        if let Some(metadata) = selected_metadata {
            metadata.check(transaction, self.session, &self.content_hash)?;
        }
        codex_title_recheck::accept(transaction, self, selected_metadata)?;
        let observed = matches!(self.guard, TrajectorySourceGuard::ObservedRevision(_));
        let title = self.title_evidence(observed);
        let trajectory_id = transaction
            .prepare_cached(UPSERT_CODEX_PARENT_SQL)
            .map_err(database_error)?
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
                    revision.file.map(Uuid::into_bytes),
                    revision.missing_ancestor.map(Uuid::into_bytes),
                    observed,
                    revision.contents.map(Uuid::into_bytes),
                    selected_metadata.is_some(),
                    title.verified,
                    title.withdrawn,
                    title.local == LocalOpenerEvidence::Current,
                    title.local == LocalOpenerEvidence::Withdrawn,
                    self.document.provider_limited,
                    self.document.can_clear_derived_title(),
                    self.is_jsonl_preview(),
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(database_error)?;
        if trajectory_id.is_none()
            && observed
            && (title.local == LocalOpenerEvidence::Current
                || title.verified
                || title.withdrawn.is_some())
        {
            self.refresh_current_title(transaction, &title)?;
        }
        if trajectory_id.is_none()
            && (selected_metadata.is_some() || self.workspace.is_some())
            && !cached_trajectory_state(transaction, self.session)?.is_some_and(|state| {
                state.source_updated_at.timestamp_millis()
                    == self.document.source_updated_at.timestamp_millis()
                    && state.source_revision == revision
                    && state.document_version == self.document.document_version
            })
        {
            return Err(StoreError::TrajectorySourceChanged);
        }
        codex_title_recheck::retire(transaction, self)?;
        self.record_workspace(transaction)?;
        Ok(trajectory_id)
    }

    fn title_evidence(&self, observed: bool) -> TrajectoryTitleWrite<'_> {
        let document = &self.document;
        let native = self.session.provider == Provider::Codex
            && document.origin == SessionTrajectoryOrigin::Native;
        let revision = document.source_revision;
        let stamped = native && observed && revision.file.is_some() && revision.contents.is_some();
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
        let current = document
            .derived_title
            .filter(|_| local != LocalOpenerEvidence::Withdrawn);
        let verified = native
            && current.is_some()
            && (document.title_is_verified || local == LocalOpenerEvidence::Current);
        TrajectoryTitleWrite {
            current,
            withdrawn,
            local,
            verified,
        }
    }

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
                     derived_title_is_local_opener = CASE WHEN ?2 THEN ?3 END
                 WHERE provider = 'codex' AND session_id = ?4 AND origin = 'native' AND codex_home = (SELECT home FROM current_codex_home)
                   AND document_version = ?5 AND source_updated_at = ?6
                   AND source_file_id = ?7 AND source_contents_id = ?8 AND missing_ancestor_id IS ?9
                   AND ((?3 AND derived_title_is_local_opener IS NOT 0) OR (?2 AND NOT ?3)
                        OR (?10 = derived_title AND (NOT derived_title_is_verified OR
                            (?11 AND derived_title_is_local_opener IS NOT 0))))
                   AND (derived_title IS NOT ?1 OR derived_title_is_verified <> ?2
                        OR derived_title_is_local_opener IS NOT (CASE WHEN ?2 THEN ?3 END))",
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
                ],
            )
            .map_err(database_error)?;
        Ok(())
    }

    fn record_workspace(&self, transaction: &Transaction<'_>) -> Result<()> {
        match self.workspace {
            Some(CodexWorkspaceUpdate::Verified(workspace)) => workspace.record(
                transaction,
                self.session,
                self.document.source_revision,
                &self.content_hash,
                false,
            ),
            Some(CodexWorkspaceUpdate::Withdrawn(evidence)) => {
                if !self.source_complete
                    || self.document.origin != SessionTrajectoryOrigin::Native
                    || !matches!(self.guard, TrajectorySourceGuard::ObservedRevision(_))
                {
                    return Err(StoreError::TrajectorySourceChanged);
                }
                CodexWorkspace {
                    source_revision: *evidence,
                    project_path: None,
                    git_branch: None,
                }
                .record(
                    transaction,
                    self.session,
                    self.document.source_revision,
                    &self.content_hash,
                    true,
                )
            }
            None => Ok(()),
        }
    }
}

const SEARCH_ACTIVE_TITLE: &str = "CASE WHEN active_projection.trajectory_id IS NULL THEN trajectories.derived_title ELSE active_projection.derived_title END";
const SEARCH_ACTIVE_CONTENTS: &str = "CASE WHEN active_projection.trajectory_id IS NULL THEN trajectories.source_contents_id ELSE gap.observed_contents END";

fn indexed_titles_sql() -> String {
    format!("SELECT trajectories.provider, trajectories.session_id,
                CASE WHEN failed.session_id IS NULL AND catalog.session_id IS NULL
                     THEN nullif({SEARCH_ACTIVE_TITLE}, '') END,
                CASE WHEN catalog.session_id IS NOT NULL THEN catalog.source_file_id
                     WHEN failed.session_id IS NOT NULL THEN failed.source_file_id ELSE trajectories.source_file_id END,
                CASE WHEN catalog.session_id IS NOT NULL THEN catalog.source_contents_id
                     WHEN failed.session_id IS NOT NULL THEN failed.source_contents_id ELSE {SEARCH_ACTIVE_CONTENTS} END,
                coalesce(active_projection.source_updated_at, trajectories.source_updated_at),
                coalesce(active_projection.document_version, trajectories.document_version),
                CASE WHEN trajectories.provider='omp' AND failed.session_id IS NULL AND catalog.session_id IS NULL
                     AND coalesce(trajectories.title_authoritative,trajectories.source_complete)
                     THEN trajectories.source_updated_submillis END,
                CASE WHEN trajectories.provider='omp' AND failed.session_id IS NULL AND catalog.session_id IS NULL
                     AND coalesce(trajectories.title_authoritative,trajectories.source_complete)
                     THEN (SELECT coalesce(c.source_updated_submillis,0) FROM scoped_session_index c
                      WHERE c.provider=trajectories.provider AND c.session_id=trajectories.session_id
                      AND c.updated_at=trajectories.source_updated_at AND
                      (c.source_updated_submillis=trajectories.source_updated_submillis OR
                       (c.source_updated_submillis IS NULL AND trajectories.source_complete))) END
             FROM visible_trajectories AS trajectories {SEARCH_GAP_JOIN}
             LEFT JOIN scoped_trajectory_failures AS failed ON ({CONFLICTING_CODEX_FAILURE}) OR ({HIDDEN_FAILED_TRAJECTORY})
             LEFT JOIN observed_session_index AS catalog ON ({CONFLICTING_CODEX_CATALOG}) OR ({CONFLICTING_FILE_CATALOG})")
}

fn indexed_title_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedTrajectoryTitle> {
    let provider = row
        .get::<_, String>(0)?
        .parse::<Provider>()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?;
    let millis = optional_timestamp_from_row(row, 5)?.ok_or(rusqlite::Error::InvalidQuery)?;
    let time = |fraction: u32| millis + chrono::Duration::nanoseconds(i64::from(fraction));
    let version = row.get(6)?;
    Ok(IndexedTrajectoryTitle {
        document_version: version,
        verified_at: row
            .get::<_, Option<u32>>(7)?
            .map(|fraction| (version, time(fraction))),
        cached_at: row.get::<_, Option<u32>>(8)?.map(time),
        session: SessionRef::new(provider, row.get::<_, String>(1)?),
        title: row.get(2)?,
        source_revision: SourceRevision {
            file: row.get::<_, Option<[u8; 16]>>(3)?.map(Uuid::from_bytes),
            contents: row.get::<_, Option<[u8; 16]>>(4)?.map(Uuid::from_bytes),
            ..Default::default()
        },
    })
}

fn ensure_session_index_columns(transaction: &Transaction<'_>) -> Result<()> {
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
    transaction
        .create_scalar_function(
            "omnis_migrated_provider_title",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                Ok(context
                    .get_raw(0)
                    .as_str()
                    .ok()
                    .and_then(omnis_core::compact_search_title))
            },
        )
        .map_err(database_error)?;
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
                               WHERE catalog.provider = doc.provider AND catalog.session_id = doc.session_id AND catalog.codex_home IS doc.codex_home
                                 AND (catalog.source_file_id IS NULL OR catalog.source_file_id IS doc.source_file_id)
                                 AND (catalog.title = doc.derived_title
                                      OR omnis_migrated_provider_title(catalog.title) = doc.derived_title)));
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
                   WHERE catalog.provider = doc.provider AND catalog.session_id = doc.session_id AND catalog.codex_home IS doc.codex_home
                     AND (catalog.source_file_id IS NULL OR catalog.source_file_id IS doc.source_file_id)
                     AND (catalog.title = doc.derived_title OR omnis_migrated_provider_title(catalog.title) = doc.derived_title)
               );"
        ).map_err(database_error)?;
        codex_workspace::recheck_legacy_projections(transaction)?;
    }
    transaction
        .remove_function("omnis_migrated_provider_title", 1)
        .map_err(database_error)?;
    Ok(())
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
                "{action} WHERE provider = ?1 AND session_id = ?2 AND (provider <> 'codex' OR codex_home = (SELECT home FROM current_codex_home))
                 AND source_updated_at IS ?3 AND document_version = ?4
                 AND source_file_id IS ?5 AND source_contents_id IS ?6 AND source_updated_submillis IS ?7"
            ),
            params![
                session.provider.to_string(),
                session.id,
                failure
                    .source_updated_at
                    .map(|time| time.timestamp_millis()),
                failure.document_version,
                failure.source_file.map(Uuid::into_bytes),
                failure.source_contents.map(Uuid::into_bytes),
                failure.source_updated_at.filter(|_|failure.source_time_precise).map(|time|source_submillis(session.provider,time)),
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn write_indexed_session(connection: &Connection, session: &IndexedSession) -> Result<()> {
    validate_session_ref(&session.session)?;
    let mut upsert = prepare_indexed_session(connection)?;
    write_indexed_session_row(&mut upsert, session, now_timestamp(), true)
}

fn cached_content_hash(connection: &Connection, session: &SessionRef) -> Result<Option<Vec<u8>>> {
    connection
        .query_row(
            "SELECT content_hash FROM visible_trajectories WHERE provider=?1 AND session_id=?2",
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
    connection
        .query_row(
            &format!(
                "SELECT {} FROM visible_trajectories AS session_trajectories WHERE provider = ?1 AND session_id = ?2",
                trajectory_state_columns()
            ),
            params![session.provider.to_string(), session.id],
            |row| state_from_row(row, 0, session.provider),
        )
        .optional()
        .map_err(database_error)
}

fn cached_session(connection: &Connection, session: &SessionRef) -> Result<Option<IndexedSession>> {
    catalog_session(connection, session, false)
}

fn catalog_session(
    connection: &Connection,
    session: &SessionRef,
    observed: bool,
) -> Result<Option<IndexedSession>> {
    let catalog = if observed {
        "observed_session_index"
    } else {
        "scoped_session_index"
    };
    connection
        .query_row(
            &format!("SELECT provider, session_id, title, project_path, git_branch,
                    created_at, updated_at, updated_at_approximate, event_count, source_updated_submillis, source_file_id, source_contents_id
             FROM {catalog} WHERE provider = ?1 AND session_id = ?2"),
            params![session.provider.to_string(), session.id],
            indexed_session_from_row,
        )
        .optional()
        .map_err(database_error)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fmt::Write as _, path::Path};

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

    fn select_codex_home(store: &mut Store, home: &Path) {
        store.codex_home = home.to_str().map(str::to_owned);
        store
            .connection
            .borrow()
            .execute("UPDATE current_codex_home SET home=?1", [&store.codex_home])
            .unwrap();
    }

    #[test]
    fn codex_workspace_survives_discovery_until_its_source_changes() {
        let temporary = tempdir().unwrap();
        let home = temporary.path();
        let mut store = Store::open(home.join("store.sqlite3")).unwrap();
        select_codex_home(&mut store, home);
        let mut header = indexed_session(Provider::Codex, "moved", "Synthetic session");
        header.updated_at = DateTime::from_timestamp_millis(77);
        let mut observed = header.clone();
        observed.project_path = Some(temporary.path().join("moved-workspace"));
        observed.git_branch = None;
        let source = temporary.path().join("rollout.jsonl");
        store
            .record_codex_workspace(&observed, home, &source, "first")
            .unwrap();
        assert_eq!(store.indexed_sessions().unwrap(), [observed.clone()]);
        let changes = store.connection.borrow().total_changes();
        store
            .record_codex_workspace(&observed, home, &source, "first")
            .unwrap();
        assert_eq!(store.connection.borrow().total_changes(), changes);
        store
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&header))
            .unwrap();
        assert_eq!(store.indexed_sessions().unwrap(), [observed.clone()]);

        let mut replaced = observed;
        replaced.project_path = None;
        let new_source = temporary.path().join("replacement.jsonl");
        store
            .record_codex_workspace(&replaced, home, &new_source, "second")
            .unwrap();
        store
            .forget_codex_workspace(home, "moved", "first")
            .unwrap();
        let workspace = store
            .indexed_codex_workspace(home, "moved")
            .unwrap()
            .unwrap();
        assert_eq!(workspace.source_stamp, "second");
        assert_eq!(workspace.source_path, new_source);
        assert_eq!(
            store.indexed_codex_workspaces(home).unwrap()["moved"],
            workspace
        );
        assert_eq!(
            store
                .indexed_sessions_for_provider(Provider::Codex)
                .unwrap(),
            [replaced]
        );
        store
            .forget_codex_workspace(home, "moved", "second")
            .unwrap();
        assert_eq!(store.indexed_sessions().unwrap(), [header]);
    }

    #[test]
    fn a_workspace_observation_cannot_borrow_another_rollouts_search_document() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        select_codex_home(&mut store, dir.path());
        let revision = |id| SourceRevision {
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(id)),
            ..SourceRevision::default()
        };
        let time = DateTime::from_timestamp_millis(12).unwrap();
        let first = IndexedSession {
            updated_at: Some(time),
            source_revision: Some(revision(10)),
            ..indexed_session(Provider::Codex, "shared", "First title")
        };
        store.upsert_indexed_session(&first).unwrap();
        store
            .upsert_trajectory_document(
                &first.session,
                &rollout_document("firstmarker", time, revision(10), true, 7),
            )
            .unwrap();
        let second = IndexedSession {
            title: Some("Second title".into()),
            project_path: Some(dir.path().join("second-workspace")),
            source_revision: Some(revision(20)),
            ..first.clone()
        };
        store
            .record_codex_workspace(
                &second,
                dir.path(),
                &dir.path().join("rollout"),
                "new stamp",
            )
            .unwrap();
        assert_eq!(
            store.cached_sessions_with_titles(None).unwrap().0,
            std::slice::from_ref(&first)
        );
        assert_eq!(
            store.search_session_trajectories("firstmarker", 5).unwrap(),
            [first.session]
        );

        store
            .connection
            .borrow()
            .execute("DELETE FROM session_index", [])
            .unwrap();
        assert_eq!(store.cached_sessions_with_titles(None).unwrap().0, [second]);
        assert!(
            store
                .search_session_trajectories("firstmarker", 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn codex_workspaces_with_the_same_id_and_timestamp_are_isolated_by_home() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("store.sqlite3");
        let home_a = temporary.path().join("codex-a");
        let home_b = temporary.path().join("codex-b");
        let mut first = Store::open(&path).unwrap();
        select_codex_home(&mut first, &home_a);
        let mut second = Store::open(&path).unwrap();
        select_codex_home(&mut second, &home_b);
        let mut a = indexed_session(Provider::Codex, "shared", "First session");
        a.updated_at = DateTime::from_timestamp_millis(77);
        let mut b = a.clone();
        b.title = Some("Second session".into());
        b.project_path = Some(temporary.path().join("second-workspace"));
        let mut disjoint = indexed_session(Provider::Codex, "only-first", "First header only");
        disjoint.updated_at = None;
        first
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&disjoint))
            .unwrap();
        assert!(
            second
                .session_index_checked_at(Provider::Codex)
                .unwrap()
                .is_none()
        );
        first
            .record_codex_workspace(&a, &home_a, &home_a.join("rollout"), "stamp")
            .unwrap();
        second
            .record_codex_workspace(&b, &home_b, &home_b.join("rollout"), "stamp")
            .unwrap();
        second
            .replace_indexed_sessions(Provider::Codex, std::slice::from_ref(&b))
            .unwrap();
        assert_eq!(
            first.indexed_sessions().unwrap(),
            [a.clone(), disjoint.clone()]
        );
        assert_eq!(second.indexed_sessions().unwrap(), [b.clone()]);

        second
            .forget_codex_workspace(&home_b, "shared", "stamp")
            .unwrap();
        let cached = second.indexed_sessions().unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].project_path, None);
        assert_eq!(cached[0].git_branch, None);
        assert_eq!(
            first.indexed_sessions().unwrap(),
            [a.clone(), disjoint.clone()]
        );
        second
            .connection
            .borrow()
            .execute(
                "UPDATE session_index SET codex_home = NULL WHERE session_id = 'shared'",
                [],
            )
            .unwrap();
        assert!(second.indexed_sessions().unwrap().is_empty());
        assert_eq!(
            first.indexed_sessions().unwrap(),
            [a.clone(), disjoint.clone()]
        );
        second
            .replace_indexed_sessions(Provider::Codex, &[])
            .unwrap();
        assert!(second.indexed_sessions().unwrap().is_empty());
        assert_eq!(first.indexed_sessions().unwrap(), [a, disjoint]);
    }

    #[test]
    fn header_only_codex_metadata_and_index_failures_survive_another_home() {
        let temporary = tempdir().unwrap();
        let open = |home: &str| {
            let mut store = Store::open(temporary.path().join("cache")).unwrap();
            store.codex_home = Some(temporary.path().join(home).to_str().unwrap().to_owned());
            store
                .connection
                .borrow()
                .execute("UPDATE current_codex_home SET home=?1", [&store.codex_home])
                .unwrap();
            store
        };
        let first = open("first");
        let second = open("second");
        let a = indexed_session(Provider::Codex, "shared", "First header");
        let mut b = a.clone();
        b.title = Some("Second header".into());
        b.project_path = Some(temporary.path().join("second-project"));
        first.upsert_indexed_session(&a).unwrap();
        second
            .merge_indexed_sessions(Provider::Codex, std::slice::from_ref(&b))
            .unwrap();

        let failure = |version| super::TrajectoryIndexFailure {
            source_file: None,
            source_contents: None,
            source_time_precise: false,
            source_updated_at: None,
            document_version: version,
        };
        first
            .record_trajectory_index_failure(&a.session, &failure(1))
            .unwrap();
        second
            .record_trajectory_index_failure(&b.session, &failure(2))
            .unwrap();
        assert_eq!(first.indexed_sessions().unwrap(), std::slice::from_ref(&a));
        assert_eq!(second.indexed_sessions().unwrap(), std::slice::from_ref(&b));
        assert_eq!(
            second.trajectory_index_failures().unwrap()[&b.session],
            failure(2)
        );

        second.clear_trajectory_index_failure(&b.session).unwrap();
        second
            .replace_indexed_sessions(Provider::Codex, &[])
            .unwrap();
        assert!(second.indexed_sessions().unwrap().is_empty());
        assert!(second.trajectory_index_failures().unwrap().is_empty());
        assert_eq!(
            first.trajectory_index_failures().unwrap()[&a.session],
            failure(1)
        );
        assert_eq!(first.indexed_sessions().unwrap(), [a]);
    }

    #[test]
    fn unowned_codex_cache_rows_require_a_new_read_before_they_are_visible() {
        let temporary = tempdir().unwrap();
        let store = Store::open(temporary.path().join("cache")).unwrap();
        let session = indexed_session(Provider::Codex, "legacy", "Old private title");
        store.upsert_indexed_session(&session).unwrap();
        let time = Utc::now();
        store
            .upsert_session_trajectory(&session.session, "Old private message", time, true)
            .unwrap();
        store.connection.borrow().execute_batch(
            "UPDATE session_trajectories SET codex_home = NULL, derived_title = 'Old private title';
             UPDATE session_index SET codex_home = NULL;"
        ).unwrap();
        assert!(store.indexed_sessions().unwrap().is_empty());
        assert!(store.trajectory_titles().unwrap().is_empty());
        assert!(
            store
                .search_session_trajectories("private", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            !store
                .session_trajectory_source_is_current(&session.session, time)
                .unwrap()
        );
        store
            .upsert_session_trajectory(&session.session, "Fresh conversation", time, true)
            .unwrap();
        assert_eq!(
            store.search_session_trajectories("fresh", 10).unwrap(),
            [session.session]
        );
        assert!(store.trajectory_titles().unwrap().is_empty());

        let legacy_alias = SessionRef::new(Provider::Codex, "imported");
        let explicit = SessionRef::new(Provider::Imported, "e50bdced-7fac-4765-be3b-54a947cd4a3c");
        let marker = "Portable bundle";
        for session in [&legacy_alias, &explicit] {
            store
                .upsert_session_trajectory_document(
                    session,
                    marker,
                    time,
                    marker.len(),
                    marker.len(),
                    "none",
                    true,
                    SessionTrajectoryOrigin::ImportedBundle,
                )
                .unwrap();
        }
        assert_eq!(
            store.search_session_trajectories("portable", 10).unwrap(),
            [explicit]
        );
    }

    #[test]
    fn codex_workspace_respects_missing_timestamps_and_partial_refreshes() {
        let temporary = tempdir().unwrap();
        let home = temporary.path();
        let mut store = Store::open(home.join("store.sqlite3")).unwrap();
        select_codex_home(&mut store, home);
        let source = temporary.path().join("rollout");
        let mut session = indexed_session(Provider::Codex, "id", "Session");
        assert!(
            store
                .record_codex_workspace(&session, home, &source, "stamp")
                .is_err()
        );
        session.updated_at = DateTime::from_timestamp_millis(7);
        session.updated_at_approximate = true;
        assert!(
            store
                .record_codex_workspace(&session, home, &source, "stamp")
                .is_err()
        );
        assert!(store.indexed_codex_workspaces(home).unwrap().is_empty());

        session.updated_at_approximate = false;
        store
            .record_codex_workspace(&session, home, &source, "stamp")
            .unwrap();
        let mut new_header = session.clone();
        new_header.project_path = Some(temporary.path().join("other"));
        new_header.updated_at = None;
        store
            .merge_indexed_sessions(Provider::Codex, &[new_header.clone()])
            .unwrap();
        assert_eq!(store.indexed_sessions().unwrap(), [new_header]);
        assert!(store.indexed_codex_workspace(home, "id").unwrap().is_some());
        store
            .replace_indexed_sessions(Provider::Codex, &[])
            .unwrap();
        assert!(store.indexed_codex_workspace(home, "id").unwrap().is_none());
        store
            .record_codex_workspace(&session, home, &source, "stamp")
            .unwrap();
        store.forget_session(&session.session).unwrap();
        assert!(store.indexed_codex_workspaces(home).unwrap().is_empty());
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
                legacy_file: None,
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
            legacy_file: None,
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
            legacy_file: None,
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
                        title_is_verified: false,
                        local_opener: crate::LocalOpenerEvidence::Unknown,
                        withdrawn_title: None,
                        source_revision: SourceRevision::default(),
                        redacted_text: preview,
                        source_updated_at,
                        source_byte_count: preview.len(),
                        projection: None,
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
    fn discovery_check_applies_only_to_the_completed_reader_and_home() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let provider = Provider::Antigravity;
        let a = directory.path().join("first");
        let b = directory.path().join("second");
        let start = Utc::now();
        let read = |version, home: &Path| {
            store
                .completed_discovery_at(provider, version, home)
                .unwrap()
        };
        let publish = |home: &Path, started| {
            store
                .record_completed_discovery(provider, 6, home, started, [])
                .unwrap();
        };

        assert_eq!(read(6, &a), None);
        publish(&a, start);
        let first = read(6, &a).unwrap();
        assert_eq!(read(5, &a), None);
        assert_eq!(read(6, &b), None);
        publish(&b, start - chrono::Duration::seconds(1));
        assert_eq!(read(6, &a), Some(first));
        assert_eq!(read(6, &b), None);
        publish(&b, first + chrono::Duration::milliseconds(1));
        assert!(read(6, &b).is_some());
        assert_eq!(read(6, &a), None);
    }

    #[test]
    fn catalog_receipts_keep_pending_and_missing_sessions_separate_after_a_restore() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let provider = Provider::Antigravity;
        let a = directory.path().join("original");
        let b = directory.path().join("restored");
        let source = a.join("transcript.jsonl");
        let mut active = indexed_session(provider, "active", "Original");
        let omitted = indexed_session(provider, "omitted", "Old");
        store
            .merge_indexed_sessions(provider, &[active.clone(), omitted])
            .unwrap();
        let start = Utc::now();
        store
            .record_completed_discovery(
                provider,
                6,
                &a,
                start,
                [(active.clone(), Some(source.as_path()))],
            )
            .unwrap();
        assert_eq!(
            store.pending_discovery_sources(provider, 6, &a).unwrap(),
            [("active".to_owned(), Some(source))]
        );
        assert!(
            store
                .excluded_catalog_ids(provider, &b)
                .unwrap()
                .contains("active")
        );
        active.title = Some("Restored".to_owned());
        store.upsert_indexed_session(&active).unwrap();
        assert!(
            store
                .completed_discovery_at(provider, 6, &a)
                .unwrap()
                .is_none()
        );
        let next = start + chrono::Duration::seconds(1);
        store
            .record_completed_discovery(provider, 6, &a, next, [])
            .unwrap();
        assert!(
            store
                .completed_discovery_at(provider, 6, &a)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .pending_discovery_sources(provider, 6, &a)
                .unwrap()
                .is_empty()
        );
        store
            .record_completed_discovery(provider, 6, &b, next, [(active, None)])
            .unwrap();
        assert!(
            store
                .excluded_catalog_ids(provider, &a)
                .unwrap()
                .contains("active")
        );
        assert!(store.excluded_catalog_ids(provider, &b).unwrap().is_empty());
    }

    #[test]
    fn antigravity_search_upgrade_preserves_full_history_until_a_full_read() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let time = Utc::now();
        let later = time + chrono::Duration::seconds(1);
        for provider in [Provider::Antigravity, Provider::AntigravityDesktop] {
            let listing = indexed_session(provider, "history", "History");
            store.upsert_indexed_session(&listing).unwrap();
            let session = listing.session;
            let searchable = |text: &str| {
                store
                    .search_session_trajectories(text, 10)
                    .unwrap()
                    .contains(&session)
            };
            let failure = crate::TrajectoryIndexFailure {
                source_updated_at: Some(time),
                source_file: None,
                source_contents: None,
                source_time_precise: true,
                document_version: 5,
            };
            store
                .record_trajectory_index_failure(&session, &failure)
                .unwrap();
            let write =
                |text: &str, document_version, source_updated_at, source_complete, strategy| {
                    store
                        .upsert_trajectory_document(
                            &session,
                            &TrajectoryDocument {
                                title_is_verified: false,
                                local_opener: crate::LocalOpenerEvidence::Unknown,
                                withdrawn_title: None,
                                redacted_text: text,
                                source_updated_at,
                                source_revision: if provider == Provider::Antigravity
                                    && document_version == 6
                                {
                                    SourceRevision {
                                        legacy_file: None,
                                        file: Some(Uuid::from_u128(1)),
                                        contents: Some(Uuid::from_u128(2)),
                                        missing_ancestor: None,
                                    }
                                } else {
                                    SourceRevision::default()
                                },
                                title_authoritative: false,
                                provider_limited: strategy == "provider_read_limit",
                                source_byte_count: text.len(),
                                projection: None,
                                indexed_byte_count: text.len(),
                                truncation_strategy: strategy,
                                source_complete,
                                origin: SessionTrajectoryOrigin::Native,
                                document_version,
                                derived_title: None,
                            },
                        )
                        .unwrap();
                };
            write("middleword", 5, time, true, "none");
            for preview_time in [time, later] {
                write("preview", 6, preview_time, false, "none");
                let state = &store.trajectory_index_states().unwrap()[&session];
                assert_eq!(state.document_version, 5);
                assert!(searchable("middleword"));
            }
            if provider == Provider::Antigravity {
                write(
                    "middleword latestword",
                    6,
                    later,
                    false,
                    "provider_read_limit",
                );
                write("preview", 6, later, false, "none");
                let state = store.trajectory_index_states().unwrap()[&session];
                assert!(state.provider_limited);
                assert!(!state.source_complete);
                assert!(searchable("latestword"));
            }
            write("upgradedword", 6, later, true, "none");
            if provider == Provider::Antigravity {
                write(
                    "sample",
                    6,
                    later + chrono::Duration::seconds(1),
                    false,
                    "none",
                );
            }
            let state = &store.trajectory_index_states().unwrap()[&session];
            assert_eq!(state.document_version, 6);
            assert!(state.source_complete);
            assert!(searchable("upgradedword"));
        }
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
                title_is_verified: false,
                local_opener: crate::LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at,
                source_byte_count: text.len(),
                projection: None,
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
                        title_is_verified: false,
                        local_opener: crate::LocalOpenerEvidence::Unknown,
                        withdrawn_title: None,
                        source_revision: SourceRevision::default(),
                        redacted_text: text,
                        source_updated_at: initial + chrono::Duration::seconds(seconds),
                        source_byte_count: text.len(),
                        projection: None,
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
            document_version: 5,
            title_authoritative: false,
            ..physical_document(text, time, SourceRevision::default())
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
        moved.project_path = Some(std::env::temp_dir().join("workspace/moved"));
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
            legacy_file: None,
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
            legacy_file: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: revision,
            redacted_text: text,
            source_updated_at: time,
            source_byte_count: text.len(),
            projection: None,
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
            legacy_file: None,
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
            legacy_file: None,
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
            legacy_file: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at: time,
            source_byte_count: text.len(),
            projection: None,
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
                title_is_verified: false,
                local_opener: crate::LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at: time,
                source_byte_count: text.len(),
                projection: None,
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
                title_is_verified: false,
                local_opener: crate::LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                source_revision: SourceRevision::default(),
                redacted_text: "row",
                source_updated_at: time,
                source_byte_count: 3,
                projection: None,
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
                title_is_verified: false,
                local_opener: crate::LocalOpenerEvidence::Unknown,
                withdrawn_title: None,
                source_revision: SourceRevision::default(),
                redacted_text: text,
                source_updated_at: time,
                source_byte_count: text.len() + usize::from(strategy == "document_head_tail"),
                projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: text.len() + 1,
            projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: str::len(text),
            projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: native_text,
            source_updated_at,
            source_byte_count: native_text.len() + 256,
            projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: imported_text,
            source_updated_at,
            source_byte_count: imported_text.len(),
            projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: text,
            source_updated_at,
            source_byte_count: text.len(),
            projection: None,
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
            title_is_verified: false,
            local_opener: crate::LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
            source_revision: SourceRevision::default(),
            redacted_text: &text,
            source_updated_at: Utc::now(),
            source_byte_count: text.len(),
            projection: None,
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
                    'claude', 'legacy', 'legacy migration searchable marker', 1, 1, 1
                );
                INSERT INTO bundles (bundle_id, bundle_json, saved_at) VALUES (
                    'synthetic-bundle',
                    '{"snapshot":{"session":{"provider":"claude","id":"legacy"}}}',
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
            .replace_indexed_sessions(Provider::Claude, &[])
            .expect("successful empty refresh");
        assert_eq!(
            store
                .search_session_trajectories("migration searchable", 10)
                .expect("bundle-protected migrated trajectory"),
            vec![SessionRef::new(Provider::Claude, "legacy")]
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
            project_path: Some(std::env::temp_dir().join("omnis-store-synthetic-project")),
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

    use super::{
        BranchHeadRestore, IndexedSession, LocalOpenerEvidence, SessionTrajectoryOrigin, Store,
        TRAJECTORY_CHUNK_BYTE_LIMIT, TRAJECTORY_CHUNK_MAX_OVERLAP_BYTES,
        TRAJECTORY_PHRASE_MAX_TOKENS, TRAJECTORY_QUERY_MAX_TOKEN_BYTES,
        TRAJECTORY_SEARCH_RESULT_LIMIT, TrajectoryDocument, state_root, trajectory_clauses,
        utf8_chunks,
    };

    fn rollout_document(
        text: &str,
        time: DateTime<Utc>,
        revision: SourceRevision,
        full: bool,
        version: u32,
    ) -> TrajectoryDocument<'_> {
        TrajectoryDocument {
            projection: None,
            title_authoritative: false,
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
            withdrawn_title: None,
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
                super::TrajectorySourceGuard::ObservedRevision(observed),
            )])
            .unwrap();
        store.trajectory_title(session).unwrap()
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
    fn partial_codex_upgrade_replaces_a_legacy_display_name_with_the_observed_opener() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path().join("store.sqlite3")).unwrap();
        let session = SessionRef::new(Provider::Codex, "named");
        let revision = SourceRevision {
            file: Some(Uuid::from_u128(42)),
            contents: Some(Uuid::from_u128(43)),
            ..SourceRevision::default()
        };
        let old = TrajectoryDocument {
            derived_title: Some("GUI name"),
            title_is_verified: true,
            ..rollout_document("older preview", Utc::now(), revision, false, 6)
        };
        save_observed_title(&store, &session, old);
        let receipt = || store.indexed_trajectory_title(&session).unwrap().unwrap();
        assert_eq!(receipt().document_version, 6);
        let upgraded = TrajectoryDocument {
            document_version: 7,
            derived_title: Some("actual opening prompt"),
            local_opener: LocalOpenerEvidence::Current,
            ..old
        };
        assert_eq!(
            save_observed_title(&store, &session, upgraded).as_deref(),
            upgraded.derived_title
        );
        assert_eq!(receipt().document_version, 7);
        let withdrawn = TrajectoryDocument {
            derived_title: None,
            withdrawn_title: upgraded.derived_title,
            local_opener: LocalOpenerEvidence::Withdrawn,
            ..upgraded
        };
        assert!(save_observed_title(&store, &session, withdrawn).is_none());
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
            let guard = super::TrajectorySourceGuard::ObservedRevision(
                store.trajectory_index_state(&session).unwrap(),
            );
            store
                .upsert_observed_trajectory_documents(&[(&session, doc, guard)])
                .unwrap();
        };
        save(TrajectoryDocument {
            truncation_strategy: "none",
            title_is_verified: true,
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
            ..incomplete
        });
        assert!(store.trajectory_title(&session).unwrap().is_none());
    }

    fn migrate_old_title_store(store: Store, path: &std::path::Path) -> Store {
        store
            .connection
            .borrow()
            .execute_batch(
                "ALTER TABLE session_trajectories DROP COLUMN derived_title_is_local_opener;
                 ALTER TABLE session_trajectories DROP COLUMN derived_title_is_verified;",
            )
            .unwrap();
        drop(store);
        Store::open(path).unwrap()
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
        store.connection.borrow().execute_batch("DROP TABLE codex_workspace_rechecks;
            CREATE TABLE codex_workspace_rechecks(trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                source_file_id BLOB NOT NULL,source_contents_id BLOB NOT NULL);").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
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
        let guard = super::TrajectorySourceGuard::ObservedRevision(
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

    fn restore_pre_revision_workspace_schema(store: &Store) {
        store
            .connection
            .borrow()
            .execute_batch(
                "DROP VIEW temp.observed_session_index;
             DROP TRIGGER codex_workspace_changed;
             DROP TRIGGER codex_workspace_removed;
             ALTER TABLE codex_workspace_observations DROP COLUMN source_file_id;
             ALTER TABLE codex_workspace_observations DROP COLUMN source_contents_id;",
            )
            .unwrap();
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
            ..rollout_document(text, time, SourceRevision::default(), true, 5)
        };
        store
            .upsert_trajectory_document(&session, &document)
            .unwrap();
        restore_pre_revision_workspace_schema(&store);
        store
            .connection
            .borrow()
            .execute_batch(
                "DROP TABLE legacy_codex_v5_projections;
             ALTER TABLE session_trajectories DROP COLUMN source_file_id;
             ALTER TABLE session_trajectories DROP COLUMN source_contents_id;
             ALTER TABLE session_trajectories DROP COLUMN missing_ancestor_id;",
            )
            .unwrap();
        let store = migrate_old_title_store(store, &path);
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
        let message = codex("message");
        let named = codex("named");
        let formatted = codex("formatted");
        let catalog_title = format!("  Saved   label   {}", "word ".repeat(80));
        let display_title = omnis_core::compact_search_title(&catalog_title).unwrap();
        let complete = codex("complete");
        let preview = codex("preview");
        let bundle = SessionRef::new(Provider::Imported, "bundle");
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
        for (id, title) in [
            ("named", "Saved request"),
            ("formatted", catalog_title.as_str()),
        ] {
            store
                .upsert_indexed_session(&IndexedSession {
                    source_revision: Some(revision),
                    ..indexed_session(Provider::Codex, id, title)
                })
                .unwrap();
        }
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
            legacy_file: None,
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
                Provider::Imported,
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

    pub(super) fn publish_workspace_fixture(
        store: &Store,
    ) -> (
        IndexedSession,
        super::CodexWorkspace,
        TrajectoryDocument<'static>,
    ) {
        let revision = SourceRevision {
            legacy_file: None,
            file: Some(Uuid::from_u128(1)),
            contents: Some(Uuid::from_u128(2)),
            missing_ancestor: None,
        };
        let original = IndexedSession {
            source_revision: Some(revision),
            project_path: Some(std::env::temp_dir().join("header")),
            git_branch: Some("initial".into()),
            ..indexed_session(Provider::Codex, "session", "Visible session")
        };
        let session = &original.session;
        let time = DateTime::from_timestamp_millis(1_000).unwrap();
        let doc = rollout_document("verified reply", time, revision, true, 5);
        let verified = super::CodexWorkspace {
            source_revision: revision,
            project_path: Some(std::env::temp_dir().join("latest")),
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
    fn a_live_codex_listing_hides_only_the_document_it_saw_replaced() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite3")).unwrap();
        let (header, _, doc) = publish_workspace_fixture(&store);
        let previous = store.indexed_session(&header.session).unwrap().unwrap();
        let selected = IndexedSession {
            source_revision: Some(SourceRevision {
                contents: Some(Uuid::from_u128(11)),
                ..doc.source_revision
            }),
            ..previous.clone()
        };
        let read = || store.indexed_session(&header.session).unwrap().unwrap();
        let matches = || {
            store
                .search_session_trajectories("verified reply", 2)
                .unwrap()
        };
        store
            .refresh_replaced_codex_session(&previous, &selected, || false)
            .unwrap();
        assert_eq!(read(), previous);
        store
            .refresh_replaced_codex_session(&previous, &selected, || true)
            .unwrap();
        assert_eq!(read(), selected);
        assert!(matches().is_empty());

        let stale = IndexedSession {
            title: Some("Obsolete candidate".into()),
            ..selected.clone()
        };
        store
            .refresh_replaced_codex_session(&previous, &stale, || true)
            .unwrap();
        assert_eq!(read(), selected);
        store
            .refresh_replaced_codex_session(&selected, &previous, || true)
            .unwrap();
        assert_eq!(matches(), [header.session]);
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
        let guard =
            TrajectorySourceGuard::ObservedRevision(store.trajectory_index_state(session).unwrap());
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
                TrajectorySourceGuard::ObservedRevision(None),
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
            project_path: Some(directory.path().join("creation")),
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
            project_path: Some(directory.path().join("latest")),
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
        let legacy_path = dir.path().join("legacy header");
        let write_legacy_header = || {
            store.connection.borrow().execute("UPDATE session_index
                SET source_file_id=NULL,source_contents_id=NULL,project_path=?1,git_branch='legacy branch'", [legacy_path.to_str()]).unwrap();
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
        assert_eq!(legacy.project_path.as_deref(), Some(legacy_path.as_path()));
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
        let native = rollout_document("native evidence", time, revision, true, 5);
        store
            .upsert_trajectory_document(&row.session, &native)
            .unwrap();
        let bundle = synthetic_bundle(row.session.clone(), directory.path());
        store.save_bundle(&bundle).unwrap();
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
            project_path: Some(directory.path().join("active-workspace")),
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
            legacy_file: None,
            file: Some(Uuid::from_u128(1)),
            contents: None,
            missing_ancestor: None,
        };
        let second = SourceRevision {
            legacy_file: None,
            file: Some(Uuid::from_u128(2)),
            contents: None,
            missing_ancestor: Some(Uuid::from_u128(3)),
        };
        let observed_write = |doc, prior| {
            store.upsert_observed_trajectory_documents(&[(
                &session,
                doc,
                super::TrajectorySourceGuard::ObservedRevision(prior),
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
            legacy_file: None,
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
                super::TrajectorySourceGuard::ObservedRevision(prior),
            )])
        };
        let prior = || store.trajectory_index_state(&session).expect("index state");
        let original = TrajectoryDocument {
            derived_title: Some("Previous title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
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
            rollout_document("active branch", newer, second, false, 2),
            before_switch,
        )
        .expect("select the other file");
        let retired = rollout_document("stale retired branch", newer, first, true, 2);
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
            legacy_file: None,
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
                super::TrajectorySourceGuard::ObservedRevision(prior),
            )])
        };
        let original = TrajectoryDocument {
            derived_title: Some("Original title"),
            title_is_verified: false,
            local_opener: LocalOpenerEvidence::Unknown,
            withdrawn_title: None,
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
            source_updated_at: Some(time),
            source_file: first.file,
            source_contents: first.contents,
            document_version: 2,
        };
        assert!(matches!(
            store.record_observed_trajectory_index_failure(
                &session,
                &failure,
                super::TrajectorySourceGuard::ObservedRevision(old_state)
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
                super::TrajectorySourceGuard::ObservedRevision(current),
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
            source_updated_at: Some(time),
            source_file: None,
            source_contents: None,
            document_version: 4,
        };
        store
            .record_trajectory_index_failure(&session, &failure)
            .expect("failure");
        restore_pre_revision_workspace_schema(&store);
        store
            .connection
            .borrow()
            .execute_batch(
                "DROP TRIGGER tracked_failure_insert;
                 DROP TRIGGER tracked_failure_update;
                 DROP TRIGGER tracked_failure_delete;
                 ALTER TABLE session_trajectories DROP COLUMN source_file_id;
                 ALTER TABLE session_trajectories DROP COLUMN missing_ancestor_id;
                 ALTER TABLE session_trajectories DROP COLUMN source_contents_id;
                 ALTER TABLE trajectory_index_failures DROP COLUMN source_file_id;
                 ALTER TABLE trajectory_index_failures DROP COLUMN source_contents_id;",
            )
            .expect("restore pre-revision schema");
        drop(store);

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
                    VALUES ('claude', 'old', 'Legacy title', 1, 0);",
            )
            .expect("old schema");
        drop(connection);

        let store = Store::open(&database).expect("upgraded store");
        let legacy = store
            .indexed_session(&SessionRef::new(Provider::Claude, "old"))
            .unwrap()
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
}
