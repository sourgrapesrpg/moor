//! Single-threaded benchmark of pushing through a (jepsen-produced) append-only workload.
//! Does not measure single-item reads, deletes, or updates, or concurrent access.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use moor_db::testing::jepsen::{History, Type, Value};
use moor_db::tuplebox::tb::{RelationInfo, TupleBox};
use moor_values::util::slice_ref::SliceRef;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

// This is a struct that tells Criterion.rs to use the "futures" crate's current-thread executor
use moor_db::tuplebox::{RelationId, Transaction};
use tokio::runtime::Runtime;

/// Build a test database with a bunch of relations
async fn test_db() -> Arc<TupleBox> {
    // Generate 10 test relations that we'll use for testing.
    let relations = (0..100)
        .map(|i| RelationInfo {
            name: format!("relation_{}", i),
            domain_type_id: 0,
            codomain_type_id: 0,
            secondary_indexed: false,
        })
        .collect::<Vec<_>>();

    TupleBox::new(1 << 24, 4096, None, &relations, 0).await
}

fn from_val(value: i64) -> SliceRef {
    SliceRef::from_bytes(&value.to_le_bytes()[..])
}
fn to_val(value: SliceRef) -> i64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(value.as_slice());
    i64::from_le_bytes(bytes)
}

fn load_history() -> Vec<History> {
    let lines = include_str!("list-append-dataset.json")
        .lines()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>();
    let events = lines
        .iter()
        .map(|l| serde_json::from_str::<History>(l).unwrap());
    events.collect::<Vec<_>>()
}

// Run the list-append workload on a single thread.
async fn list_append_workload(
    db: Arc<TupleBox>,
    events: &Vec<History>,
    processes: &mut HashMap<i64, Arc<Transaction>>,
) {
    for e in events {
        match e.r#type {
            Type::invoke => {
                // Start a transaction.
                let tx = Arc::new(db.clone().start_tx());
                let existing = processes.insert(e.process, tx.clone());
                assert!(
                    existing.is_none(),
                    "T{} already exists uncommitted",
                    e.process
                );
                // Execute the actions
                for ev in &e.value {
                    match ev {
                        Value::append(_, register, value) => {
                            // Insert the value into the relation.
                            let relation = RelationId(*register as usize);
                            tx.clone()
                                .relation(relation)
                                .await
                                .insert_tuple(from_val(*value), from_val(*value))
                                .await
                                .unwrap();
                        }
                        Value::r(_, register, _) => {
                            let relation = RelationId(*register as usize);

                            // Full-scan.
                            tx.relation(relation)
                                .await
                                .predicate_scan(&|_| true)
                                .await
                                .unwrap();
                        }
                    }
                }
            }
            Type::ok => {
                let tx = processes.remove(&e.process).unwrap();
                tx.commit().await.unwrap();
            }
            Type::fail => {
                let tx = processes.remove(&e.process).unwrap();
                tx.rollback().await.unwrap();
            }
        }
    }
}
async fn do_insert_workload(iters: u64, events: &Vec<History>) -> Duration {
    let mut cumulative = Duration::new(0, 0);
    for _ in 0..iters {
        // We create a brand new db for each iteration, so we have a clean slate.
        let db = test_db().await;

        // Where to track the transactions running.
        let mut processes = HashMap::new();

        let start = Instant::now();
        list_append_workload(db, events, &mut processes).await;
        black_box(());
        cumulative += start.elapsed();
    }
    cumulative
}

// Measure the # of commit/rollbacks per second using the list-append Jepsen workload.
pub fn workload_commits(c: &mut Criterion) {
    let events = load_history();

    // Count the # of commit/rollback (unique transactions) in the workload.
    let tx_count = events.iter().filter(|e| e.r#type == Type::invoke).count();
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("throughput");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(criterion::Throughput::Elements(tx_count as u64));
    group.bench_function("commit_rate", |b| {
        b.to_async(&rt)
            .iter_custom(|iters| do_insert_workload(iters, &events));
    });
    group.finish();
}

criterion_group!(benches, workload_commits);
criterion_main!(benches);