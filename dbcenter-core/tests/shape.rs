//! What the collection must answer, whatever shape the delta layer has.
//!
//! `_base` and `_adds` were arms of a `UNION ALL` view, and the view decided
//! which of two versions of a row a reader saw: a row present in both was shown
//! once, from `_adds`, and a row marked in `_dels` was not shown at all. Making
//! them children of one table moves that decision out of the reader and into
//! the writer, where a mistake is a duplicated or resurrected row rather than a
//! slow query.
//!
//! So these tests say nothing about the shape. They ask the collection the
//! questions a caller would ask, and they passed before the change as well as
//! after it - which is the only reason they are worth having.

use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use dbcenter_core::DBCenter;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn node(dir: &Path, name: &str, store: &Path) -> (Arc<DBCenter>, SyncEngine) {
    let db = DBCenter::builder()
        .path(dir.join(name).to_str().unwrap())
        .persist(true)
        .multi_user_sync_mode(true)
        .build()
        .expect("embedded postgres should start");
    let adapter: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(LocalStorageAdapter::new(store.to_path_buf())));
    let engine = SyncEngine::new(Arc::clone(&db), adapter);
    (db, engine)
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbcenter_shape_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Force the pending row to a known id, so both nodes can be made to hold their
/// own version of the same row without racing `gen_random_uuid`.
fn pin(db: &DBCenter, table: &str, id: &str, updated_at: &str) {
    db.query(
        &format!(
            "UPDATE \"{table}_adds\" SET id = '{id}'::uuid, \
             updated_at = '{updated_at}'::timestamptz"
        ),
        &[],
    )
    .unwrap();
}

const ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

/// A row can sit in `_base` and `_adds` at once: the bucket has one version and
/// this node has edited it since. The reader must see one row, and it must be
/// the local edit - the version that has not been pushed yet is the newer one.
///
/// The view did this by hiding the `_base` arm. Children of one table are both
/// scanned, so the duplicate has to be prevented on the way in instead.
#[test]
fn a_pulled_row_does_not_duplicate_an_edit_this_node_has_not_pushed() {
    let dir = workspace("duplicate");
    let store_a = dir.join("store-a");
    std::fs::create_dir_all(&store_a).unwrap();

    let (a, a_sync) = node(&dir, "a", &store_a);
    a.collection("orders")
        .insert_one(&json!({"ref": "in-the-bucket"}))
        .unwrap();
    pin(&a, "orders", ID, "2026-01-01 09:00:00+00");
    a_sync.sync_table("orders", 100).unwrap();

    // B holds its own, unsynced, edit of the same row.
    let (b, _) = node(&dir, "b", &dir.join("store-b"));
    b.collection("orders")
        .insert_one(&json!({"ref": "edited-here"}))
        .unwrap();
    pin(&b, "orders", ID, "2026-01-01 10:00:00+00");

    let b_reads_a = SyncEngine::new(
        Arc::clone(&b),
        Arc::new(Box::new(LocalStorageAdapter::new(store_a.clone()))),
    );
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();

    let rows = b.query("SELECT \"ref\" AS r FROM orders", &[]).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the row is in the collection twice: {rows:?}"
    );
    assert_eq!(
        rows[0]["r"],
        json!("edited-here"),
        "the pulled version displaced an edit this node had not pushed yet"
    );

    a.shutdown().unwrap();
    b.shutdown().unwrap();
}

/// A row deleted here and still present in the bucket must stay deleted. The
/// view read `_dels` on every query to arrange that; nothing reads it now, so
/// the restore has to.
#[test]
fn a_row_deleted_here_does_not_come_back_from_the_bucket() {
    let dir = workspace("resurrect");
    let store_a = dir.join("store-a");
    std::fs::create_dir_all(&store_a).unwrap();

    let (a, a_sync) = node(&dir, "a", &store_a);
    a.collection("orders")
        .insert_many(&[json!({"ref": "keep"}), json!({"ref": "remove"})])
        .unwrap();
    a_sync.sync_table("orders", 100).unwrap();

    let (b, _) = node(&dir, "b", &dir.join("store-b"));
    let b_reads_a = SyncEngine::new(
        Arc::clone(&b),
        Arc::new(Box::new(LocalStorageAdapter::new(store_a.clone()))),
    );
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();
    assert_eq!(
        b.query("SELECT * FROM orders", &[]).unwrap().len(),
        2,
        "the pull did not bring both rows"
    );

    let remove: HashMap<String, JsonValue> =
        [("ref".to_string(), json!("remove"))].into_iter().collect();
    b.collection("orders").delete_many(Some(&remove)).unwrap();
    assert_eq!(b.query("SELECT * FROM orders", &[]).unwrap().len(), 1);

    // The same file again. The deleted row is still in it.
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();

    let rows = b.query("SELECT \"ref\" AS r FROM orders", &[]).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a row deleted here came back on the next pull: {rows:?}"
    );
    assert_eq!(rows[0]["r"], json!("keep"));

    a.shutdown().unwrap();
    b.shutdown().unwrap();
}

/// An update writes the new version into `_adds`; the old one must stop being
/// visible whether it was in `_adds` or already compacted into `_base`.
#[test]
fn updating_a_compacted_row_leaves_one_version_of_it() {
    let dir = workspace("update");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let (db, sync) = node(&dir, "a", &store);
    db.collection("orders")
        .insert_one(&json!({"ref": "first", "n": 1}))
        .unwrap();
    // Pushing moves the row out of _adds and into _base.
    sync.sync_table("orders", 100).unwrap();
    assert_eq!(
        db.query("SELECT * FROM orders_adds", &[]).unwrap().len(),
        0,
        "the push should have emptied _adds"
    );

    let which: HashMap<String, JsonValue> =
        [("ref".to_string(), json!("first"))].into_iter().collect();
    let set: HashMap<String, JsonValue> = [("$set".to_string(), json!({"n": 2}))]
        .into_iter()
        .collect();
    db.collection("orders").update_many(&which, &set).unwrap();

    let rows = db
        .query("SELECT \"n\" AS n FROM orders ORDER BY n", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the row is visible in both its versions: {rows:?}"
    );
    assert_eq!(rows[0]["n"], json!(2));

    db.shutdown().unwrap();
}

/// The reason for the change. A hundred rows by timestamp must not sort the
/// other 199,900 - the plan has to stop when it has what it was asked for.
///
/// Asserted on the plan rather than on a duration, because a duration on a busy
/// machine says nothing. `Merge Append` over the two children is the shape that
/// makes an ordered read of a delta collection cost what it costs PostgreSQL.
#[test]
fn an_ordered_limited_query_stops_when_it_has_enough_rows() {
    let dir = workspace("ordered");
    let (db, _) = node(&dir, "a", &dir.join("store"));

    let batch: Vec<_> = (0..5000).map(|i| json!({"n": i})).collect();
    db.collection("orders").insert_many(&batch).unwrap();
    db.query("ANALYZE orders_base", &[]).unwrap();
    db.query("ANALYZE orders_adds", &[]).unwrap();

    let plan = db
        .query(
            "EXPLAIN (ANALYZE, TIMING OFF, FORMAT JSON) \
             SELECT * FROM orders ORDER BY created_at DESC LIMIT 100",
            &[],
        )
        .unwrap();
    let text = plan[0].to_string();

    let scanned: u64 = text
        .match_indices("\"Actual Rows\":")
        .filter_map(|(i, _)| {
            text[i + 14..]
                .trim_start()
                .split(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
                .and_then(|n| n.split('.').next())
                .and_then(|n| n.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(u64::MAX);

    assert!(
        scanned < 1000,
        "returning 100 rows walked {scanned} of them - the ordering is not \
         being pushed into the children:\n{text}"
    );

    db.shutdown().unwrap();
}
