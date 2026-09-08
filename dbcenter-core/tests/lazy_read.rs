//! How much of a remote table does a lookup actually read?
//!
//! The pitch is that a query touches a fraction of the data rather than
//! downloading it. That is a claim about bytes, so it is measured here rather
//! than asserted.

use dbcenter_core::storage::{CountingStorage, LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::SyncEngine;
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
