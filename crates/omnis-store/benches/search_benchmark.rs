use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use omnis_ir::{Provider, SessionRef};
use omnis_store::Store;
use tempfile::{TempDir, tempdir};

const DATASET_SIZES: [usize; 2] = [1_000, 10_000];
const SEARCH_LIMIT: usize = 100;
const ONE_TERM_QUERY: &str = "lattice";
const AND_QUERY: &str = "lattice checkpoint";
const PHRASE_QUERY: &str = "\"lattice checkpoint\"";

struct Dataset {
    _directory: TempDir,
    path: PathBuf,
}

impl Dataset {
    fn create(document_count: usize) -> Self {
        let directory = tempdir().expect("create benchmark directory");
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).expect("open benchmark store");
        let source_updated_at = Utc
            .timestamp_millis_opt(1_700_000_000_000)
            .single()
            .expect("valid benchmark timestamp");

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

        Self {
            _directory: directory,
            path,
        }
    }

    fn open(&self) -> Store {
        Store::open(&self.path).expect("open benchmark store")
    }
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

fn bench_searches(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("session_trajectory_search");

    for document_count in DATASET_SIZES {
        let dataset = Dataset::create(document_count);
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
                        black_box(
                            store
                                .search_session_trajectories(query, SEARCH_LIMIT)
                                .expect("search benchmark store"),
                        );
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
}

criterion_group!(benches, bench_searches);
criterion_main!(benches);
