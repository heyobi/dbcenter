//! The physical shape of a delta-mode collection.
//!
//! A collection used to be a view, `_adds UNION ALL (_base minus _adds minus
//! _dels)`, and PostgreSQL cannot push an ORDER BY ... LIMIT through the
//! anti-join arm: a hundred rows by timestamp sorted the whole table. It is now
//! a table partitioned on a `_layer` column, with `_base` and `_adds` as its
//! partitions, so the planner merges two index scans and stops early.
//!
//! The cost is that nothing makes an id unique across the two partitions. The
//! view picked the `_adds` version when an id sat in both; a partitioned table
//! shows both. These pin the places that could put an id in both, and the
//! deletes the view used to hide with `_dels`.

use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use dbcenter_core::DBCenter;
use serde_json::json;
use std::collections::HashMap;
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
    let dir = std::env::temp_dir().join(format!("dbcenter_layout_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Give the pending row a known id and timestamp.
fn pin(db: &DBCenter, table: &str, id: &str, updated_at: &str) {
    db.query(
        &format!(
            "UPDATE \"{table}_adds\" SET id = '{id}'::uuid, updated_at = '{updated_at}'::timestamptz"
        ),
        &[],
    )
    .unwrap();
}

fn refs_for(db: &DBCenter, table: &str, id: &str) -> Vec<serde_json::Value> {
    db.query(
        &format!("SELECT \"ref\" AS r FROM \"{table}\" WHERE id = '{id}'::uuid"),
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|row| row["r"].clone())
    .collect()
}

const ID: &str = "11111111-2222-3333-4444-555555555555";

#[test]
fn a_pulled_row_never_shows_twice_beside_a_pending_local_edit() {
    let dir = workspace("pending");
    let store_a = dir.join("store-a");
    std::fs::create_dir_all(&store_a).unwrap();
    let store_b = dir.join("store-b");
    std::fs::create_dir_all(&store_b).unwrap();

    let a = node(&dir, "a", &store_a);
    a.db.collection("orders")
        .insert_one(&json!({"ref": "from-a"}))
        .unwrap();
    pin(&a.db, "orders", ID, "2026-01-01 10:00:00+00");
    a.engine.sync_table("orders", 100).unwrap();

    // B has its own edit of the same row, not yet synced: it sits in _adds.
    let b = node(&dir, "b", &store_b);
    b.db.collection("orders")
        .insert_one(&json!({"ref": "from-b"}))
        .unwrap();
    pin(&b.db, "orders", ID, "2026-01-01 09:00:00+00");

    let b_reads_a = SyncEngine::new(
        Arc::clone(&b.db),
        Arc::new(Box::new(LocalStorageAdapter::new(store_a.clone()))),
    );
    b_reads_a.pull_and_apply("orders", SyncMode::Full).unwrap();

    // Writing A's row into _base would leave the id in both partitions, and
    // the collection would show it twice. The pending local edit is what the
    // view used to show, so it is what stays.
    assert_eq!(
        refs_for(&b.db, "orders", ID),
        vec![json!("from-b")],
        "the row should appear once, as the pending local edit"
    );

    // The version that was not taken is recorded, like any other loser.
    let conflicts =
        b.db.query(
            "SELECT incoming_won, incoming->>'ref' AS lost FROM orders_conflicts",
            &[],
        )
        .unwrap();
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0]["incoming_won"], json!(false));
    assert_eq!(conflicts[0]["lost"], json!("from-a"));

    // And syncing the local edit afterwards still leaves exactly one row.
    b.engine.sync_table("orders", 100).unwrap();
    assert_eq!(refs_for(&b.db, "orders", ID), vec![json!("from-b")]);

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}

#[test]
fn a_pull_does_not_bring_back_a_row_deleted_here() {
    let dir = workspace("deleted");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    a.db.collection("orders")
        .insert_one(&json!({"ref": "doomed"}))
        .unwrap();
    pin(&a.db, "orders", ID, "2026-01-01 10:00:00+00");
    a.engine.sync_table("orders", 100).unwrap();

    let b = node(&dir, "b", &store);
    b.engine.pull_and_apply("orders", SyncMode::Full).unwrap();
    assert_eq!(refs_for(&b.db, "orders", ID), vec![json!("doomed")]);

    let mut query = HashMap::new();
    query.insert("ref".to_string(), json!("doomed"));
    assert_eq!(b.db.collection("orders").delete_one(&query).unwrap(), 1);
    assert!(refs_for(&b.db, "orders", ID).is_empty());

    // The view hid a deleted row behind its marker in _dels while the row
    // itself stayed in _base. Now the row is really gone, so the marker is the
    // only thing standing between it and the same file applied again.
    b.engine.pull_and_apply("orders", SyncMode::Full).unwrap();
    assert!(
        refs_for(&b.db, "orders", ID).is_empty(),
        "a pull resurrected a row this node deleted"
    );

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}

#[test]
fn an_update_to_a_synced_row_is_pending_again_and_shows_once() {
    let dir = workspace("update");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    let orders = a.db.collection("orders");
    orders.insert_one(&json!({"ref": "v1"})).unwrap();
    pin(&a.db, "orders", ID, "2026-01-01 10:00:00+00");
    a.engine.sync_table("orders", 100).unwrap();

    // Synced, so the row lives in _base now.
    let pending =
        a.db.query("SELECT count(*) AS n FROM orders_adds", &[])
            .unwrap();
    assert_eq!(pending[0]["n"], json!(0));

    // An update through the collection - including a field the collection has
    // never seen, which has to reach both partitions - must leave the row
    // where the next sync will find it. Updated in place inside _base, it
    // would never be synced again.
    let mut query = HashMap::new();
    query.insert("ref".to_string(), json!("v1"));
    let mut update = HashMap::new();
    update.insert(
        "$set".to_string(),
        json!({"ref": "v2", "note": "new field"}),
    );
    assert_eq!(orders.update_one(&query, &update).unwrap(), 1);

    assert_eq!(refs_for(&a.db, "orders", ID), vec![json!("v2")]);
    let pending =
        a.db.query("SELECT \"ref\" AS r, note FROM orders_adds", &[])
            .unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["note"], json!("new field"));

    // An update issued as plain SQL, the way a wire-protocol client would, is
    // routed the same way.
    a.engine.sync_table("orders", 100).unwrap();
    a.db.query(
        &format!("UPDATE orders SET \"ref\" = 'v3' WHERE id = '{ID}'::uuid"),
        &[],
    )
    .unwrap();
    assert_eq!(refs_for(&a.db, "orders", ID), vec![json!("v3")]);
    a.engine.sync_table("orders", 100).unwrap();

    // A node starting from the bucket sees the last version, once.
    let b = node(&dir, "b", &store);
    b.engine.pull_and_apply("orders", SyncMode::Full).unwrap();
    assert_eq!(refs_for(&b.db, "orders", ID), vec![json!("v3")]);

    a.db.shutdown().unwrap();
    b.db.shutdown().unwrap();
}

#[test]
fn an_explicit_id_written_over_sql_replaces_rather_than_duplicates() {
    let dir = workspace("explicit");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    a.db.collection("orders")
        .insert_one(&json!({"ref": "old"}))
        .unwrap();
    pin(&a.db, "orders", ID, "2026-01-01 10:00:00+00");
    a.engine.sync_table("orders", 100).unwrap();

    // insert_one never passes an id through, but a client on the wire can.
    // The view answered this with the _adds row; so must the table.
    a.db.query(
        &format!("INSERT INTO orders (id, \"ref\") VALUES ('{ID}'::uuid, 'new')"),
        &[],
    )
    .unwrap();
    assert_eq!(refs_for(&a.db, "orders", ID), vec![json!("new")]);

    // The check runs once per statement, so it has to cover every row of one.
    a.engine.sync_table("orders", 100).unwrap();
    a.db.query(
        &format!(
            "INSERT INTO orders (id, \"ref\") VALUES \
             (gen_random_uuid(), 'other'), ('{ID}'::uuid, 'newer')"
        ),
        &[],
    )
    .unwrap();
    assert_eq!(refs_for(&a.db, "orders", ID), vec![json!("newer")]);

    a.db.shutdown().unwrap();
}

#[test]
fn an_ordered_limited_read_merges_index_scans_instead_of_sorting() {
    let dir = workspace("ordered");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    let orders = a.db.collection("orders");
    orders
        .insert_many(&(0..2000).map(|i| json!({"n": i})).collect::<Vec<_>>())
        .unwrap();
    a.engine.sync_table("orders", 1000).unwrap();
    // Half in _base, half still pending, so both partitions take part.
    let split =
        a.db.query("SELECT count(*) AS n FROM orders_adds", &[])
            .unwrap();
    assert_eq!(split[0]["n"], json!(1000));

    a.db.query("CREATE INDEX ON orders (created_at)", &[])
        .unwrap();
    a.db.query("ANALYZE orders", &[]).unwrap();

    let plan: String =
        a.db.query(
            "EXPLAIN (COSTS OFF) SELECT * FROM orders ORDER BY created_at DESC LIMIT 10",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| row["QUERY PLAN"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains("Merge Append"), "{plan}");
    assert!(
        !plan.contains("Sort\n") && !plan.trim_start().starts_with("Sort"),
        "the whole collection is being sorted to return ten rows:\n{plan}"
    );

    let newest =
        a.db.query(
            "SELECT n FROM orders ORDER BY created_at DESC, n DESC LIMIT 1",
            &[],
        )
        .unwrap();
    assert_eq!(newest[0]["n"], json!(1999));

    a.db.shutdown().unwrap();
}

/// The shape every delta collection had before it was partitioned, built by
/// hand: one row only in _base, one updated (in _adds, its old version still in
/// _base and marked in _dels), one deleted (in _base, marked in _dels), and one
/// inserted since the last sync.
fn build_view_shaped(db: &DBCenter, name: &str) {
    for sql in [
        r#"CREATE TABLE {n}_base (id uuid PRIMARY KEY, data jsonb,
             created_at timestamptz DEFAULT CURRENT_TIMESTAMP,
             updated_at timestamptz DEFAULT CURRENT_TIMESTAMP, "ref" text)"#,
        r#"CREATE TABLE {n}_adds (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), data jsonb,
             created_at timestamptz DEFAULT CURRENT_TIMESTAMP,
             updated_at timestamptz DEFAULT CURRENT_TIMESTAMP, "ref" text)"#,
        "CREATE TABLE {n}_dels (id uuid PRIMARY KEY, deleted_at timestamptz DEFAULT CURRENT_TIMESTAMP)",
        r#"CREATE VIEW {n} AS SELECT * FROM {n}_adds a UNION ALL
             SELECT * FROM {n}_base b
             WHERE NOT EXISTS (SELECT 1 FROM {n}_adds x WHERE x.id = b.id)
               AND NOT EXISTS (SELECT 1 FROM {n}_dels y WHERE y.id = b.id)"#,
        r#"INSERT INTO {n}_base (id, "ref") VALUES
             ('00000000-0000-0000-0000-000000000001', 'kept'),
             ('00000000-0000-0000-0000-000000000002', 'stale'),
             ('00000000-0000-0000-0000-000000000003', 'deleted')"#,
        r#"INSERT INTO {n}_adds (id, "ref") VALUES
             ('00000000-0000-0000-0000-000000000002', 'updated'),
             ('00000000-0000-0000-0000-000000000004', 'new')"#,
        r#"INSERT INTO {n}_dels (id) VALUES
             ('00000000-0000-0000-0000-000000000002'),
             ('00000000-0000-0000-0000-000000000003')"#,
    ] {
        db.query(&sql.replace("{n}", name), &[]).unwrap();
    }
}

fn visible(db: &DBCenter, name: &str) -> Vec<serde_json::Value> {
    db.query(
        &format!("SELECT \"ref\" AS r FROM {name} ORDER BY \"ref\""),
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|row| row["r"].clone())
    .collect()
}

fn relkind(db: &DBCenter, name: &str) -> serde_json::Value {
    db.query(
        &format!("SELECT relkind::text AS k FROM pg_class WHERE relname = '{name}'"),
        &[],
    )
    .unwrap()[0]["k"]
        .clone()
}

const WHAT_THE_VIEW_SHOWED: [&str; 3] = ["kept", "new", "updated"];

#[test]
fn a_collection_still_shaped_as_a_view_is_converted_without_losing_rows() {
    let dir = workspace("legacy");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let a = node(&dir, "a", &store);

    build_view_shaped(&a.db, "legacy");
    let before = visible(&a.db, "legacy");
    assert_eq!(before, WHAT_THE_VIEW_SHOWED.map(|r| json!(r)).to_vec());

    // Opening the collection converts it.
    a.db.collection("legacy");

    assert_eq!(relkind(&a.db, "legacy"), json!("p"), "still a view");
    assert_eq!(
        visible(&a.db, "legacy"),
        before,
        "the conversion changed what is visible"
    );

    // The rows the view was hiding are gone, so nothing is in both partitions.
    let base =
        a.db.query("SELECT count(*) AS n FROM legacy_base", &[])
            .unwrap();
    assert_eq!(base[0]["n"], json!(1));

    // Converting is a one-time thing: opening it again changes nothing.
    a.db.collection("legacy");
    assert_eq!(visible(&a.db, "legacy"), before);

    // And it syncs like any other collection.
    assert_eq!(a.engine.sync_table("legacy", 100).unwrap(), 2);
    assert_eq!(visible(&a.db, "legacy"), before);

    a.db.shutdown().unwrap();
}

#[test]
fn a_view_shaped_collection_is_converted_when_the_database_opens() {
    // A client on the wire protocol never goes through `collection()`, so
    // waiting for that would leave its collections as views for good.
    let dir = workspace("legacy_reopen");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    build_view_shaped(&a.db, "legacy");
    a.db.shutdown().unwrap();
    drop(a);

    let a = node(&dir, "a", &store);
    assert_eq!(relkind(&a.db, "legacy"), json!("p"), "still a view");
    assert_eq!(
        visible(&a.db, "legacy"),
        WHAT_THE_VIEW_SHOWED.map(|r| json!(r)).to_vec()
    );
    a.db.shutdown().unwrap();
}

#[test]
fn a_delta_collection_works_on_postgresql_12() {
    // The id default, gen_random_uuid(), is built in from 13; on 12 it comes
    // from pgcrypto, and creating the first collection used to fail.
    let dir = workspace("pg12");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let db = DBCenter::builder()
        .path(dir.join("a").to_str().unwrap())
        .persist(true)
        .multi_user_sync_mode(true)
        .postgres_version("12")
        .build()
        .expect("embedded postgres 12 should start");
    let adapter: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(LocalStorageAdapter::new(store.clone())));
    let engine = SyncEngine::new(Arc::clone(&db), adapter);

    let orders = db.collection("orders");
    orders.insert_one(&json!({"ref": "v1"})).unwrap();
    // A client on the wire leaves the id to the column default.
    db.query(
        "INSERT INTO orders (\"ref\") VALUES ('wire') RETURNING id",
        &[],
    )
    .unwrap();
    assert_eq!(engine.sync_table("orders", 100).unwrap(), 2);

    let mut query = HashMap::new();
    query.insert("ref".to_string(), json!("v1"));
    let mut update = HashMap::new();
    update.insert("$set".to_string(), json!({"ref": "v2"}));
    assert_eq!(orders.update_one(&query, &update).unwrap(), 1);
    assert_eq!(orders.count(None).unwrap(), 2);
    db.shutdown().unwrap();
}

#[test]
fn a_sync_gives_back_the_space_it_empties_in_adds() {
    // Every scan of the collection reads _adds too, so empty pages left there
    // by the move cost every aggregate, not just disk.
    let dir = workspace("adds_space");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();

    let a = node(&dir, "a", &store);
    a.db.collection("events")
        .insert_one(&json!({"n": 0, "pad": "x"}))
        .unwrap();
    a.db.query(
        "INSERT INTO events_adds (id, n, pad) \
         SELECT gen_random_uuid(), g, md5(g::text) FROM generate_series(1, 20000) g",
        &[],
    )
    .unwrap();
    assert_eq!(a.engine.sync_table("events", 100_000).unwrap(), 20_001);

    let size =
        a.db.query("SELECT pg_relation_size('events_adds') AS b", &[])
            .unwrap();
    assert_eq!(size[0]["b"], json!(0), "_adds kept its empty pages");
    a.db.shutdown().unwrap();
}
