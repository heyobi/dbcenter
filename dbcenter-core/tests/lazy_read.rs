//! How much of a remote table does a lookup actually read?
//!
//! The pitch is that a query touches a fraction of the data rather than
//! downloading it. That is a claim about bytes, so it is measured here rather
//! than asserted.

use dbcenter_core::storage::{CountingStorage, LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use dbcenter_core::DBCenter;
use serde_json::json;
use std::sync::Arc;

/// A database in delta mode with a counting adapter over local storage.
fn setup(name: &str) -> (Arc<DBCenter>, Arc<CountingStorage>, SyncEngine) {
    let dir = std::env::temp_dir().join(format!("dbcenter_lazy_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("storage")).unwrap();

    let db = DBCenter::new(
        Some(dir.join("db").to_str().unwrap()),
        true,
        false,
        None,
        None,
        None,
        None,
        true, // delta mode: what SyncEngine works against
    )
    .expect("embedded postgres should start");

    let local: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(LocalStorageAdapter::new(dir.join("storage"))));
    let counting = Arc::new(CountingStorage::new(local));
    let as_adapter: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(CountingHandle(Arc::clone(&counting))));

    let engine = SyncEngine::new(Arc::clone(&db), as_adapter);
    (db, counting, engine)
}

/// Lets the engine hold one adapter while the test keeps the counters.
struct CountingHandle(Arc<CountingStorage>);

impl StorageAdapter for CountingHandle {
    fn upload(&self, k: &str, d: &[u8]) -> dbcenter_core::Result<()> {
        self.0.upload(k, d)
    }
    fn download(&self, k: &str) -> dbcenter_core::Result<Vec<u8>> {
        self.0.download(k)
    }
    fn list(&self, p: &str) -> dbcenter_core::Result<Vec<String>> {
        self.0.list(p)
    }
    fn size(&self, k: &str) -> dbcenter_core::Result<u64> {
        self.0.size(k)
    }
    fn download_range(&self, k: &str, s: u64, l: u64) -> dbcenter_core::Result<Vec<u8>> {
        self.0.download_range(k, s, l)
    }
    fn delete(&self, k: &str) -> dbcenter_core::Result<()> {
        self.0.delete(k)
    }
    fn lock(&self, k: &str, t: u64, w: &str) -> dbcenter_core::Result<bool> {
        self.0.lock(k, t, w)
    }
    fn refresh_lock(&self, k: &str, t: u64, w: &str) -> dbcenter_core::Result<bool> {
        self.0.refresh_lock(k, t, w)
    }
    fn unlock(&self, k: &str, w: &str) -> dbcenter_core::Result<()> {
        self.0.unlock(k, w)
    }
    fn rename(&self, a: &str, b: &str) -> dbcenter_core::Result<()> {
        self.0.rename(a, b)
    }
}

#[test]
fn a_remote_lookup_reads_a_fraction_of_the_table() {
    let (db, counting, engine) = setup("fraction");

    // Comfortably more than one row group, or there is nothing to prune and the
    // measurement says nothing.
    let collection = db.collection("events");
    for chunk in 0..60 {
        let docs: Vec<_> = (0..1000)
            .map(|i| {
                let n = chunk * 1000 + i;
                json!({
                    "seq": n,
                    "payload": format!("{:0>200}", n),   // padding, so the file is not tiny
                })
            })
            .collect();
        collection.insert_many(&docs).unwrap();
    }

    engine.sync_table("events", 200_000).unwrap();
    engine.vacuum("events").unwrap();

    // A real id: delta mode generates UUIDs, so a made-up one matches nothing
    // and the measurement would be of a lookup that found nothing.
    let target: String = db
        .query("SELECT id::text AS id FROM events_base LIMIT 1", &[])
        .unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let objects = counting.list("events").unwrap();
    let total: u64 = objects.iter().filter_map(|k| counting.size(k).ok()).sum();
    assert!(total > 0, "nothing was written to storage");

    // Everything up to here is setup; only the lookup should be counted.
    counting.reset();

    let hits = engine.scan_smart("events", &target).unwrap();
    let read = counting.bytes_downloaded();

    println!(
        "table {} bytes across {} object(s); lookup read {} bytes in {} request(s) -> {:.1}%",
        total,
        objects.len(),
        read,
        counting.read_requests(),
        read as f64 / total as f64 * 100.0
    );

    assert!(!hits.is_empty(), "the row should have been found");

    // 60,000 rows at 20,000 to a row group is three groups, and a lookup for one
    // id should touch one of them plus the footer. Half the table is a generous
    // bound on that; before the reader was made lazy this read 100%.
    //
    // The fraction falls as the table grows, because a row group holds a fixed
    // number of rows: the same lookup against ten times the data still reads one
    // group. That is the property the "query it without downloading it" claim
    // rests on, and it is why this is measured rather than asserted.
    assert!(
        read < total / 2,
        "a lookup read {read} bytes of a {total} byte table; row-group pruning is not working"
    );

    // Chatty is its own failure: a read per page costs latency even when it
    // costs no traffic.
    assert!(
        counting.read_requests() < 50,
        "a single lookup issued {} requests",
        counting.read_requests()
    );
}

#[test]
fn a_remote_lookup_sees_rows_written_since_the_last_compaction() {
    // A compaction writes <table>_BASE_...; every push after it writes
    // <table>_DELTA_.... Reading only the newest base - which is what this did -
    // silently misses everything written since, and reports it as "not found".
    let (db, _counting, engine) = setup("deltas");

    let collection = db.collection("orders");
    collection
        .insert_many(&[json!({"ref": "before-vacuum"})])
        .unwrap();
    engine.sync_table("orders", 10_000).unwrap();
    engine.vacuum("orders").unwrap();

    // Written after the compaction, so it lives in a delta object only.
    collection
        .insert_many(&[json!({"ref": "after-vacuum"})])
        .unwrap();
    engine.sync_table("orders", 10_000).unwrap();

    // The field is read from its own column, not out of `data`. A document
    // inserted through the collection API lives in the row's columns; `data` is
    // for the layouts that genuinely store JSON there, and duplicating the
    // document into it is what made the Parquet twice the size it needed to be.
    let rows = db
        .query(
            "SELECT id::text AS id, \"ref\" AS r FROM orders_base ORDER BY created_at",
            &[],
        )
        .unwrap();
    let ids: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["r"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "both rows should be in the local base: {ids:?}"
    );

    for (id, marker) in &ids {
        let found = engine.scan_smart("orders", id).unwrap();
        assert_eq!(
            found.len(),
            1,
            "'{marker}' was written but a remote lookup cannot find it"
        );

        // The shape has to be usable. It used to come back as col_0, col_2, col_3.
        let doc = &found[0];
        assert_eq!(
            doc["ref"],
            json!(marker),
            "the document's own fields belong at the top level: {doc:?}"
        );
        assert!(doc.get("_id").is_some(), "no _id in {doc:?}");
    }

    db.shutdown().unwrap();
}

#[test]
fn a_document_survives_the_round_trip_through_storage() {
    // Write through the collection API, sync, and read it back on a database
    // that has never seen it. This is the whole premise - the machine is
    // disposable, the bucket is not - and it is the path the earlier cluster
    // tests missed, because those inserted JSONB through psql directly.
    let dir = std::env::temp_dir().join("dbcenter_roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("storage")).unwrap();

    let storage = || -> Arc<Box<dyn StorageAdapter + Send + Sync>> {
        Arc::new(Box::new(LocalStorageAdapter::new(dir.join("storage"))))
    };

    let source = DBCenter::new(
        Some(dir.join("a").to_str().unwrap()),
        true,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
    source
        .collection("invoices")
        .insert_many(&[
            json!({"ref": "INV-1", "amount": 19.99, "paid": true}),
            json!({"ref": "INV-2", "amount": 5.0, "paid": false}),
        ])
        .unwrap();
    SyncEngine::new(Arc::clone(&source), storage())
        .sync_table("invoices", 10_000)
        .unwrap();
    source.shutdown().unwrap();

    // A different database, nothing in common but the bucket.
    let target = DBCenter::new(
        Some(dir.join("b").to_str().unwrap()),
        true,
        false,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
    SyncEngine::new(Arc::clone(&target), storage())
        .pull_and_apply("invoices", SyncMode::Full)
        .unwrap();

    let mut docs = target.collection("invoices").find(None).unwrap();
    docs.sort_by_key(|d| d["ref"].as_str().unwrap_or_default().to_string());

    assert_eq!(docs.len(), 2, "restored: {docs:?}");
    assert_eq!(
        docs[0]["ref"],
        json!("INV-1"),
        "fields must come back at the top level, not nested: {:?}",
        docs[0]
    );
    assert_eq!(docs[0]["amount"], json!(19.99));
    assert_eq!(docs[0]["paid"], json!(true));
    assert_eq!(docs[1]["ref"], json!("INV-2"));

    target.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn synced_parquet_has_a_column_per_field() {
    // The whole document used to be serialised into a single `data` string,
    // which left Parquet with nothing to be columnar about: a query that wanted
    // one field still read every byte of every document and parsed JSON out of
    // it. Measured over five million rows that made reads off object storage
    // 1.8x slower than PostgreSQL at 2.1x the stored bytes - see
    // docs/columnar-reads.md. This pins the shape that fixed it.
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let (db, _counting, engine) = setup("columnar");
    let collection = db.collection("readings");
    collection
        .insert_many(&[
            json!({"sensor": "a", "celsius": 21.5, "count": 3, "ok": true}),
            json!({"sensor": "b", "celsius": 19.0, "count": 7, "ok": false}),
        ])
        .unwrap();
    engine.sync_table("readings", 10_000).unwrap();

    let dir = std::env::temp_dir().join("dbcenter_lazy_columnar/storage");
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".parquet"))
        .expect("a sync should have written a parquet file");

    let reader = SerializedFileReader::new(std::fs::File::open(&file).unwrap()).unwrap();
    let schema = reader.metadata().file_metadata().schema_descr();
    let columns: Vec<(String, String)> = (0..schema.num_columns())
        .map(|i| {
            let c = schema.column(i);
            (c.name().to_string(), format!("{:?}", c.physical_type()))
        })
        .collect();

    for field in ["sensor", "celsius", "count", "ok"] {
        assert!(
            columns.iter().any(|(n, _)| n == field),
            "'{field}' should be its own parquet column, not buried in a JSON string: {columns:?}"
        );
    }

    // And with the type it had, so an aggregate does not have to parse strings.
    let typed = |name: &str| {
        columns
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
            .unwrap()
    };
    assert_eq!(typed("celsius"), "DOUBLE", "a float field stays a float");
    assert_eq!(typed("count"), "INT64", "an integer field stays an integer");
    assert_eq!(typed("ok"), "BOOLEAN", "a boolean field stays a boolean");

    // The round trip still produces the document that went in.
    let found = collection.find(None).unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn an_updated_row_survives_being_synced() {
    // Updating a row goes through the view's INSTEAD OF trigger, which marks the
    // id in _dels so the version still in _base is hidden, and puts the new
    // version in _adds. Syncing then moves that new version into _base - and
    // left the marker behind, so the view went on hiding a row that was now the
    // current one. Update, sync, and the row was gone from the collection.
    let (db, _counting, engine) = setup("updates");
    let collection = db.collection("accounts");
    collection
        .insert_many(&[json!({"name": "ada", "balance": 100})])
        .unwrap();
    engine.sync_table("accounts", 10_000).unwrap();

    db.query("UPDATE accounts SET balance = 250 WHERE name = 'ada'", &[])
        .unwrap();
    assert_eq!(collection.count(None).unwrap(), 1, "before the sync");

    engine.sync_table("accounts", 10_000).unwrap();

    assert_eq!(
        collection.count(None).unwrap(),
        1,
        "the row is still there after syncing the update"
    );
    let rows = db.query("SELECT balance FROM accounts", &[]).unwrap();
    assert_eq!(rows[0]["balance"], json!(250), "and it is the new value");
}

/// The footer cache exists so a second lookup does not re-read the metadata.
///
/// It never worked: the cache was written only when a read ended exactly at the
/// end of the file and was longer than eight bytes, and Parquet's metadata block
/// ends eight bytes before the end. The file was never created, so every lookup
/// paid for the footer again.
#[test]
fn a_second_lookup_reads_less_because_the_footer_is_cached() {
    let (db, counting, engine) = setup("footer_cache");

    let collection = db.collection("events");
    let docs: Vec<_> = (0..20_000)
        .map(|n| json!({ "seq": n, "payload": format!("{:0>200}", n) }))
        .collect();
    collection.insert_many(&docs).unwrap();
    engine.sync_table("events", 50_000).unwrap();
    engine.vacuum("events").unwrap();

    let target: String = db
        .query("SELECT id::text AS id FROM events_base LIMIT 1", &[])
        .unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    counting.reset();
    let first = engine.scan_smart("events", &target).unwrap();
    let cold = counting.bytes_downloaded();

    counting.reset();
    let second = engine.scan_smart("events", &target).unwrap();
    let warm = counting.bytes_downloaded();

    println!("cold lookup {cold} bytes, warm lookup {warm} bytes");

    assert_eq!(first, second, "the cached footer changed the answer");
    assert!(!first.is_empty(), "the row should have been found");

    let meta_dir = db.get_path().join(".meta");
    let cached: Vec<_> = std::fs::read_dir(&meta_dir)
        .expect("the .meta directory should exist")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!cached.is_empty(), "nothing was cached in {meta_dir:?}");
    for entry in &cached {
        assert!(
            entry.metadata().unwrap().len() > 8,
            "{:?} holds only the offset header",
            entry.path()
        );
    }

    assert!(
        warm < cold,
        "the second lookup read {warm} bytes against the first one's {cold}; the footer cache is not being used"
    );

    db.shutdown().unwrap();
}
