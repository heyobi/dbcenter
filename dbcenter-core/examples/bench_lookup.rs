//! How long does one row take to come back from object storage?
//!
//! Two ways of asking for one id that lives only in the bucket:
//!
//! - `scan_smart`, which keeps each file's footer on local disk and fetches
//!   only the row group that can hold the id;
//! - the columnar view, which hands the query to DuckDB.
//!
//! Each against the bucket directly and through a second endpoint, which can
//! be a proxy that delays every packet - object storage a network away.
//!
//!     cargo run --release --example bench_lookup -- <direct-endpoint> <slow-endpoint> [rows]
use dbcenter_core::storage::StorageAdapter;
use dbcenter_core::sync_engine::SyncEngine;
use dbcenter_core::vfs::s3::{S3Config, S3StorageAdapter};
use dbcenter_core::DBCenter;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Counts every request by kind, so a slow lookup says where its time went.
#[derive(Default)]
struct Tally {
    list: AtomicU64,
    size: AtomicU64,
    range: AtomicU64,
    bytes: AtomicU64,
}

impl Tally {
    fn take(&self) -> String {
        format!(
            "{} list, {} size, {} range reads, {} KB",
            self.list.swap(0, Ordering::Relaxed),
            self.size.swap(0, Ordering::Relaxed),
            self.range.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed) / 1024
        )
    }
}

struct Counted {
    inner: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    tally: Arc<Tally>,
}

impl StorageAdapter for Counted {
    fn upload(&self, key: &str, data: &[u8]) -> dbcenter_core::error::Result<()> {
        self.inner.upload(key, data)
    }
    fn download(&self, key: &str) -> dbcenter_core::error::Result<Vec<u8>> {
        let d = self.inner.download(key)?;
        self.tally.range.fetch_add(1, Ordering::Relaxed);
        self.tally
            .bytes
            .fetch_add(d.len() as u64, Ordering::Relaxed);
        Ok(d)
    }
    fn list(&self, prefix: &str) -> dbcenter_core::error::Result<Vec<String>> {
        self.tally.list.fetch_add(1, Ordering::Relaxed);
        self.inner.list(prefix)
    }
    fn size(&self, key: &str) -> dbcenter_core::error::Result<u64> {
        self.tally.size.fetch_add(1, Ordering::Relaxed);
        self.inner.size(key)
    }
    fn download_range(
        &self,
        key: &str,
        start: u64,
        length: u64,
    ) -> dbcenter_core::error::Result<Vec<u8>> {
        let d = self.inner.download_range(key, start, length)?;
        self.tally.range.fetch_add(1, Ordering::Relaxed);
        self.tally
            .bytes
            .fetch_add(d.len() as u64, Ordering::Relaxed);
        Ok(d)
    }
    fn delete(&self, key: &str) -> dbcenter_core::error::Result<()> {
        self.inner.delete(key)
    }
    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> dbcenter_core::error::Result<bool> {
        self.inner.lock(key, ttl_ms, worker_id)
    }
    fn refresh_lock(
        &self,
        key: &str,
        ttl_ms: u64,
        worker_id: &str,
    ) -> dbcenter_core::error::Result<bool> {
        self.inner.refresh_lock(key, ttl_ms, worker_id)
    }
    fn unlock(&self, key: &str, worker_id: &str) -> dbcenter_core::error::Result<()> {
        self.inner.unlock(key, worker_id)
    }
    fn rename(&self, src: &str, dest: &str) -> dbcenter_core::error::Result<()> {
        self.inner.rename(src, dest)
    }
    fn duckdb_access(&self) -> Option<dbcenter_core::storage::DuckDbAccess> {
        self.inner.duckdb_access()
    }
}

fn storage(endpoint: &str) -> Arc<Box<dyn StorageAdapter + Send + Sync>> {
    Arc::new(Box::new(
        S3StorageAdapter::with_prefix(
            S3Config {
                bucket: "bench".into(),
                region: "us-east-1".into(),
                endpoint: endpoint.into(),
                access_key: "dbcenter".into(),
                secret_key: "dbcenter123".into(),
                session_token: None,
                path_style: true,
            },
            "lookup",
        )
        .unwrap(),
    ))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let direct = args
        .get(1)
        .cloned()
        .unwrap_or("http://127.0.0.1:29000".into());
    let slow = args
        .get(2)
        .cloned()
        .unwrap_or("http://127.0.0.1:29100".into());
    let rows: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(200_000);

    let dir = std::env::temp_dir().join("dbcenter_bench_lookup");
    let _ = std::fs::remove_dir_all(&dir);
    let db = DBCenter::builder()
        .path(&dir)
        .persist(true)
        .multi_user_sync_mode(true)
        .columnar(true)
        .build()
        .unwrap();

    let setup = storage(&direct);
    for key in setup.list("events").unwrap() {
        setup.delete(&key).unwrap();
    }
    let engine = SyncEngine::new(Arc::clone(&db), Arc::clone(&setup));

    // A compacted base, then two deltas after it: what a table looks like
    // between compactions.
    db.collection("events")
        .insert_one(&json!({"user_id": 0, "region": "x", "amount": 0.0, "payload": "x"}))
        .unwrap();
    db.query(
        &format!(
            "INSERT INTO events_adds (id, user_id, region, amount, payload) \
             SELECT gen_random_uuid(), (random() * 100000)::bigint, 'north', random() * 1000, \
                    md5(g::text) || md5((g * 7)::text) FROM generate_series(1, {rows}) g"
        ),
        &[],
    )
    .unwrap();
    engine.sync_table("events", rows + 10).unwrap();
    engine.vacuum("events").unwrap();
    for _ in 0..2 {
        db.query(
            "INSERT INTO events_adds (id, user_id, region, amount, payload) \
             SELECT gen_random_uuid(), 1, 'south', 1.0, 'late' FROM generate_series(1, 100)",
            &[],
        )
        .unwrap();
        engine.sync_table("events", 1000).unwrap();
    }
    let files = setup.list("events").unwrap();
    println!("{rows} rows; objects in the bucket: {}", files.len());

    let ids: Vec<String> = db
        .query(
            "SELECT id::text AS id FROM events_base ORDER BY random() LIMIT 21",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();

    for (label, endpoint) in [("bucket next door", &direct), ("bucket 20 ms away", &slow)] {
        let tally = Arc::new(Tally::default());
        let counted: Arc<Box<dyn StorageAdapter + Send + Sync>> = Arc::new(Box::new(Counted {
            inner: storage(endpoint),
            tally: Arc::clone(&tally),
        }));
        let engine = SyncEngine::new(Arc::clone(&db), counted);

        // Cold: no footer on local disk yet.
        let _ = std::fs::remove_dir_all(db.get_path().join(".meta"));
        let t = Instant::now();
        let found = engine.scan_smart("events", &ids[0]).unwrap();
        let cold = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(found.len(), 1, "id not found");
        let cold_requests = tally.take();

        let mut warm = Vec::new();
        for id in &ids[1..] {
            let t = Instant::now();
            let found = engine.scan_smart("events", id).unwrap();
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(found.len(), 1, "id not found");
        }
        let warm_requests = tally.take();

        engine.ensure_columnar_view("events").unwrap();
        let mut duck = Vec::new();
        for id in &ids[1..11] {
            let t = Instant::now();
            let got = db
                .query(
                    &format!("SELECT * FROM events_columnar WHERE id = '{id}'"),
                    &[],
                )
                .unwrap();
            duck.push(t.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(got.len(), 1, "columnar view did not find the id");
        }

        let local = {
            let mut v = Vec::new();
            for id in &ids[1..11] {
                let t = Instant::now();
                db.query(&format!("SELECT * FROM events WHERE id = '{id}'"), &[])
                    .unwrap();
                v.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            median(v)
        };

        println!("\n{label}:");
        println!("  scan_smart, first lookup (footers fetched): {cold:8.1} ms  [{cold_requests}]");
        println!(
            "  scan_smart, later lookups (median of 20):   {:8.1} ms  [20 lookups: {warm_requests}]",
            median(warm)
        );
        println!(
            "  columnar view through DuckDB (median of 10): {:8.1} ms",
            median(duck)
        );
        println!("  local table, for reference (median of 10):  {local:8.2} ms");
    }

    db.shutdown().unwrap();
}
