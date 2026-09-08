//! What happens when two nodes edit the same row.
//!
//! Resolution is last-write-wins on `updated_at`, which means one of the two
//! edits is not in the database afterwards. These check the two things that
//! were wrong with it: that the loser was dropped without trace, and that two
//! edits in the same clock tick left the nodes holding different data forever.

use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use dbcenter_core::DBCenter;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Node {
    db: Arc<DBCenter>,
    engine: SyncEngine,
}

fn node(dir: &Path, name: &str, store: &Path) -> Node {
    let db = DBCenter::builder()
        .path(dir.join(name).to_str().unwrap())
        .persist(true)
        .multi_user_sync_mode(true)
        .build()
        .expect("embedded postgres should start");

    let adapter: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(LocalStorageAdapter::new(store.to_path_buf())));
    let engine = SyncEngine::new(Arc::clone(&db), adapter);
    Node { db, engine }
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbcenter_conflict_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Force the pending row to a known id and timestamp, so a test can decide
/// which of two edits is the later one instead of racing the clock.
///
/// This runs against `_adds`, before the sync moves the row out, so the Parquet
/// the other node reads carries the pinned values.
fn pin(db: &DBCenter, table: &str, id: &str, updated_at: &str) {
    db.query(
        &format!(
            "UPDATE \"{table}_adds\" SET id = '{id}'::uuid, updated_at = '{updated_at}'::timestamptz"
        ),
        &[],
    )
    .unwrap();
}

const ID: &str = "11111111-2222-3333-4444-555555555555";

#[test]
fn the_edit_that_loses_is_kept_where_it_can_be_found() {
    let dir = workspace("recorded");
    let store_a = dir.join("store-a");
    let store_b = dir.join("store-b");
    std::fs::create_dir_all(&store_a).unwrap();
    std::fs::create_dir_all(&store_b).unwrap();

    // Each node writes its own version of the same row. A's is the later one.
    let a = node(&dir, "a", &store_a);
    a.db.collection("orders")
        .insert_one(&json!({"ref": "from-a"}))
        .unwrap();
    pin(&a.db, "orders", ID, "2026-01-01 10:00:00+00");
    a.engine.sync_table("orders", 100).unwrap();

    let b = node(&dir, "b", &store_b);
    b.db.collection("orders")
        .insert_one(&json!({"ref": "from-b"}))
        .unwrap();
    pin(&b.db, "orders", ID, "2026-01-01 09:00:00+00");
    b.engine.sync_table("orders", 100).unwrap();

    let b_reads_a = SyncEngine::new(
        Arc::clone(&b.db),
        Arc::new(Box::new(LocalStorageAdapter::new(store_a.clone()))),
    );
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();

    let rows =
        b.db.query("SELECT \"ref\" AS r FROM orders_base", &[])
            .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["r"],
        json!("from-a"),
        "the newer edit should have won"
    );

    let conflicts =
        b.db.query(
            "SELECT incoming_won, existing->>'ref' AS lost FROM orders_conflicts",
            &[],
        )
        .unwrap();
    assert_eq!(
        conflicts.len(),
        1,
        "the overwrite went unrecorded: {conflicts:?}"
    );
    assert_eq!(conflicts[0]["incoming_won"], json!(true));
    assert_eq!(
        conflicts[0]["lost"],
        json!("from-b"),
        "the discarded version is not the one that was there"
    );

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}

#[test]
fn applying_the_same_file_twice_records_nothing() {
    let dir = workspace("idempotent");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    a.db.collection("orders")
        .insert_many(&(0..20).map(|i| json!({"n": i})).collect::<Vec<_>>())
        .unwrap();
    a.engine.sync_table("orders", 100).unwrap();

    let b = node(&dir, "b", &store);
    b.engine.pull_and_apply("orders", SyncMode::Full).unwrap();
    b.engine.pull_and_apply("orders", SyncMode::Full).unwrap();

    let count =
        b.db.query("SELECT count(*) AS n FROM orders_conflicts", &[])
            .unwrap();
    assert_eq!(
        count[0]["n"],
        json!(0),
        "re-applying an unchanged file logged a conflict"
    );
    let rows =
        b.db.query("SELECT count(*) AS n FROM orders_base", &[])
            .unwrap();
    assert_eq!(rows[0]["n"], json!(20));

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}

#[test]
fn two_edits_in_the_same_tick_leave_both_nodes_holding_the_same_row() {
    // The comparison used to be a plain `>`, so neither node accepted the
    // other's row and the two stayed different for good. The tie is broken on a
    // hash of the row, which both nodes compute identically.
    let dir = workspace("tie");
    let store_a = dir.join("store-a");
    let store_b = dir.join("store-b");
    std::fs::create_dir_all(&store_a).unwrap();
    std::fs::create_dir_all(&store_b).unwrap();

    let same_moment = "2026-01-01 12:00:00+00";

    let a = node(&dir, "a", &store_a);
    a.db.collection("orders")
        .insert_one(&json!({"ref": "written-by-a"}))
        .unwrap();
    pin(&a.db, "orders", ID, same_moment);
    a.engine.sync_table("orders", 100).unwrap();

    let b = node(&dir, "b", &store_b);
    b.db.collection("orders")
        .insert_one(&json!({"ref": "written-by-b"}))
        .unwrap();
    pin(&b.db, "orders", ID, same_moment);
    b.engine.sync_table("orders", 100).unwrap();

    // Each node takes in the other's version.
    let a_reads_b = SyncEngine::new(
        Arc::clone(&a.db),
        Arc::new(Box::new(LocalStorageAdapter::new(store_b.clone()))),
    );
    let b_reads_a = SyncEngine::new(
        Arc::clone(&b.db),
        Arc::new(Box::new(LocalStorageAdapter::new(store_a.clone()))),
    );
    a_reads_b.pull_and_apply("orders", SyncMode::Full).unwrap();
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();

    let on_a =
        a.db.query("SELECT \"ref\" AS r FROM orders_base", &[])
            .unwrap();
    let on_b =
        b.db.query("SELECT \"ref\" AS r FROM orders_base", &[])
            .unwrap();

    assert_eq!(on_a.len(), 1, "{on_a:?}");
    assert_eq!(on_b.len(), 1, "{on_b:?}");
    assert_eq!(
        on_a[0]["r"], on_b[0]["r"],
        "the nodes disagree about a row they both edited in the same tick"
    );

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}
