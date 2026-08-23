use std::{fs, path::PathBuf};

use chrono::{TimeZone, Utc};
use criterion::{BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main};
use omnis_ir::{Provider, SessionRef};
use omnis_store::{IndexedSession, SessionTrajectoryOrigin, Store};
use tempfile::{TempDir, tempdir};

const DATASET_SIZES: [usize; 2] = [1_000, 10_000];
const SEARCH_LIMIT: usize = 100;
const RANKED_SEARCH_LIMIT: usize = 10;
const OVERSIZED_SEARCH_LIMIT: usize = 10_000;
const MAX_RANKED_RESULTS: usize = 512;
const REFRESH_SESSION_COUNT: usize = 1_000;
const RETAINED_SESSION_COUNT: usize = 750;
const MEBIBYTE: usize = 1024 * 1024;
const HEAD_TAIL_EDGE_BYTES: usize = 5 * MEBIBYTE;
const HEAD_TAIL_INDEXED_BYTES: usize = 2 * HEAD_TAIL_EDGE_BYTES;
const HEAD_MARKER: &str = "benchmark-head-marker";
const TAIL_MARKER: &str = "benchmark-tail-marker";
const ONE_TERM_QUERY: &str = "lattice";
const AND_QUERY: &str = "lattice checkpoint";
const PHRASE_QUERY: &str = "\"lattice checkpoint\"";

struct Dataset {
    directory: TempDir,
    path: PathBuf,
}

impl Dataset {
    fn create(document_count: usize) -> Self {
        let dataset = Self::empty();
        let store = dataset.open();
        let source_updated_at = benchmark_timestamp();

        for index in 0..document_count {
            let session = SessionRef::new(Provider::Codex, format!("benchmark-{index}"));
            store
                .upsert_session_trajectory(
                    &session,
                    &synthetic_document(index),
                    source_updated_at,
                    true,
                )
                .expect("index synthetic trajectory");
        }
        drop(store);
        dataset
    }

    fn empty() -> Self {
        let directory = tempdir().expect("create benchmark directory");
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).expect("open benchmark store");
        drop(store);
        Self { directory, path }
    }

    fn open(&self) -> Store {
        Store::open(&self.path).expect("open benchmark store")
    }

    fn into_store(self) -> BenchmarkStore {
        let store = Store::open(&self.path).expect("open benchmark store");
        BenchmarkStore {
            store,
            _directory: self.directory,
        }
    }

    fn copy_store(&self) -> BenchmarkStore {
        let directory = tempdir().expect("create copied benchmark directory");
        let path = directory.path().join("store.sqlite3");
        fs::copy(&self.path, &path).expect("copy benchmark store");
        let store = Store::open(path).expect("open copied benchmark store");
        BenchmarkStore {
            store,
            _directory: directory,
        }
    }
}

struct BenchmarkStore {
    store: Store,
    _directory: TempDir,
}

struct RefreshDataset {
    dataset: Dataset,
    all_sessions: Vec<IndexedSession>,
    retained_sessions: Vec<IndexedSession>,
}

impl RefreshDataset {
    fn create() -> Self {
        let dataset = Dataset::empty();
        let store = dataset.open();
        let source_updated_at = benchmark_timestamp();
        let all_sessions = (0..REFRESH_SESSION_COUNT)
            .map(synthetic_indexed_session)
            .collect::<Vec<_>>();

        store
            .replace_indexed_sessions(Provider::Claude, &all_sessions)
            .expect("seed provider index");
        for (index, indexed) in all_sessions.iter().enumerate() {
            let text = format!(
                "Synthetic refresh trajectory {index} contains deterministic pruning content."
            );
            store
                .upsert_session_trajectory_document(
                    &indexed.session,
                    &text,
                    source_updated_at,
                    text.len(),
                    text.len(),
                    "none",
                    true,
                    SessionTrajectoryOrigin::Native,
                )
                .expect("seed refresh trajectory");
        }
        drop(store);

        let retained_sessions = all_sessions[..RETAINED_SESSION_COUNT].to_vec();
        Self {
            dataset,
            all_sessions,
            retained_sessions,
        }
    }
}

fn benchmark_timestamp() -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000)
        .single()
        .expect("valid benchmark timestamp")
}

fn synthetic_document(index: usize) -> String {
    let phrase = if index % 4 == 0 {
        "lattice checkpoint"
    } else {
        "lattice review checkpoint"
    };
    format!(
        "Synthetic trajectory {index} records lattice search coverage and {phrase}; the checkpoint remains deterministic for benchmark measurements."
    )
}

fn synthetic_indexed_session(index: usize) -> IndexedSession {
    let timestamp = benchmark_timestamp();
    IndexedSession {
        session: SessionRef::new(Provider::Claude, format!("refresh-{index:04}")),
        title: Some(format!("Synthetic refresh session {index}")),
        project_path: None,
        git_branch: Some("benchmark".to_owned()),
        created_at: Some(timestamp),
        updated_at: Some(timestamp),
        updated_at_approximate: false,
        event_count: index + 1,
    }
}

fn sized_segment(prefix: &str, suffix: &str, byte_count: usize) -> String {
    const FILLER: &str = " synthetic lattice checkpoint record remains deterministic. ";
    assert!(prefix.len() + suffix.len() <= byte_count);
    let mut segment = String::with_capacity(byte_count);
    segment.push_str(prefix);
    while segment.len() + FILLER.len() + suffix.len() <= byte_count {
        segment.push_str(FILLER);
    }
    let remainder = byte_count - segment.len() - suffix.len();
    segment.push_str(&FILLER[..remainder]);
    segment.push_str(suffix);
    segment
}

fn synthetic_head_tail_document() -> String {
    let head = sized_segment(HEAD_MARKER, "\n\n", HEAD_TAIL_EDGE_BYTES);
    let tail = sized_segment("", TAIL_MARKER, HEAD_TAIL_EDGE_BYTES);
    let mut document = String::with_capacity(HEAD_TAIL_INDEXED_BYTES);
    document.push_str(&head);
    document.push_str(&tail);
    assert_eq!(document.len(), HEAD_TAIL_INDEXED_BYTES);
    document
}

fn index_head_tail_document(store: &Store, document: &str) {
    store
        .upsert_session_trajectory_document(
            &SessionRef::new(Provider::Codex, "ten-mib-head-tail"),
            document,
            benchmark_timestamp(),
            HEAD_TAIL_INDEXED_BYTES + 2 * MEBIBYTE,
            HEAD_TAIL_INDEXED_BYTES,
            "document_head_tail",
            true,
            SessionTrajectoryOrigin::Native,
        )
        .expect("index ten MiB head-tail document");
}

fn bench_searches(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("session_trajectory_search");
    let datasets = DATASET_SIZES.map(Dataset::create);

    for (document_count, dataset) in DATASET_SIZES.into_iter().zip(&datasets) {
        group.throughput(Throughput::Elements(document_count as u64));
        for (query_name, query) in [
            ("one_term", ONE_TERM_QUERY),
            ("and", AND_QUERY),
            ("phrase", PHRASE_QUERY),
        ] {
            let cold_id = format!("{document_count}/cold/{query_name}");
            group.bench_function(cold_id, |bencher| {
                bencher.iter_batched(
                    || dataset.open(),
                    |store| {
                        let matches = store
                            .search_session_trajectories(query, SEARCH_LIMIT)
                            .expect("search benchmark store");
                        (store, black_box(matches))
                    },
                    BatchSize::SmallInput,
                );
            });

            let warm_store = dataset.open();
            warm_store
                .search_session_trajectories(query, SEARCH_LIMIT)
                .expect("warm benchmark query");
            let warm_id = format!("{document_count}/warm/{query_name}");
            group.bench_function(warm_id, |bencher| {
                bencher.iter(|| {
                    black_box(
                        warm_store
                            .search_session_trajectories(query, SEARCH_LIMIT)
                            .expect("search benchmark store"),
                    );
                });
            });
        }
    }

    group.finish();
    bench_bounded_ranked_searches(criterion, &datasets[1]);
}

fn bench_bounded_ranked_searches(criterion: &mut Criterion, dataset: &Dataset) {
    let mut group = criterion.benchmark_group("session_trajectory_ranked_page");
    group.throughput(Throughput::Elements(10_000));

    for (limit_name, limit, expected_count) in [
        ("limit_10", RANKED_SEARCH_LIMIT, RANKED_SEARCH_LIMIT),
        (
            "request_10000_capped_512",
            OVERSIZED_SEARCH_LIMIT,
            MAX_RANKED_RESULTS,
        ),
    ] {
        let validation_page = dataset
            .open()
            .search_session_trajectory_page(AND_QUERY, limit)
            .expect("validate bounded ranked page");
        assert_eq!(validation_page.matches.len(), expected_count);
        assert!(validation_page.has_more);

        let cold_id = format!("connection_cold/{limit_name}");
        group.bench_function(cold_id, |bencher| {
            bencher.iter_batched(
                || dataset.open(),
                |store| {
                    let page = store
                        .search_session_trajectory_page(AND_QUERY, limit)
                        .expect("query ranked page");
                    (store, black_box(page))
                },
                BatchSize::SmallInput,
            );
        });

        let warm_store = dataset.open();
        warm_store
            .search_session_trajectory_page(AND_QUERY, limit)
            .expect("warm ranked query");
        let warm_id = format!("connection_warm/{limit_name}");
        group.bench_function(warm_id, |bencher| {
            bencher.iter(|| {
                black_box(
                    warm_store
                        .search_session_trajectory_page(AND_QUERY, limit)
                        .expect("query ranked page"),
                );
            });
        });
    }

    group.finish();
}

fn bench_head_tail_indexing(criterion: &mut Criterion) {
    let document = synthetic_head_tail_document();
    let validation_dataset = Dataset::empty();
    let validation_store = validation_dataset.open();
    index_head_tail_document(&validation_store, &document);
    for marker in [HEAD_MARKER, TAIL_MARKER] {
        let matches = validation_store
            .search_session_trajectories(marker, 1)
            .expect("validate head-tail marker search");
        assert_eq!(matches.len(), 1);
    }

    let mut group = criterion.benchmark_group("session_trajectory_index");
    group.throughput(Throughput::Bytes(HEAD_TAIL_INDEXED_BYTES as u64));
    group.bench_function("document_head_tail_10_mib", |bencher| {
        bencher.iter_batched(
            || Dataset::empty().into_store(),
            |benchmark_store| {
                index_head_tail_document(&benchmark_store.store, black_box(&document));
                benchmark_store
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_provider_refreshes(criterion: &mut Criterion) {
    let fixture = RefreshDataset::create();
    let validation = fixture.dataset.copy_store();
    validation
        .store
        .replace_indexed_sessions(Provider::Claude, &fixture.retained_sessions)
        .expect("validate provider refresh pruning");
    assert!(
        validation
            .store
            .session_trajectory_source_is_current(
                &fixture.retained_sessions[0].session,
                benchmark_timestamp(),
            )
            .expect("validate retained trajectory")
    );
    assert!(
        !validation
            .store
            .session_trajectory_source_is_current(
                &fixture.all_sessions[RETAINED_SESSION_COUNT].session,
                benchmark_timestamp(),
            )
            .expect("validate pruned trajectory")
    );
    let mut group = criterion.benchmark_group("provider_index_refresh");

    group.throughput(Throughput::Elements(REFRESH_SESSION_COUNT as u64));
    let unchanged_store = fixture.dataset.open();
    group.bench_function("1000/unchanged", |bencher| {
        bencher.iter(|| {
            unchanged_store
                .replace_indexed_sessions(Provider::Claude, black_box(&fixture.all_sessions))
                .expect("refresh unchanged provider index");
        });
    });
    drop(unchanged_store);

    group.throughput(Throughput::Elements(REFRESH_SESSION_COUNT as u64));
    group.bench_function("1000/prune_250_stale_native", |bencher| {
        bencher.iter_batched(
            || fixture.dataset.copy_store(),
            |copied| {
                copied
                    .store
                    .replace_indexed_sessions(
                        Provider::Claude,
                        black_box(&fixture.retained_sessions),
                    )
                    .expect("refresh and prune provider index");
                copied
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_searches,
    bench_head_tail_indexing,
    bench_provider_refreshes
);
criterion_main!(benches);
