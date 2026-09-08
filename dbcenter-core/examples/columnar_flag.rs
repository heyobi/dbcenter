//! One flag, and the columnar view is simply there.
use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::SyncEngine;
use dbcenter_core::DBCenter;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let dir = std::env::temp_dir().join("dbcenter_columnar_flag");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("storage")).unwrap();

    let t = Instant::now();
    let db = DBCenter::builder()
        .path(dir.join("db"))
        .persist(true)
        .multi_user_sync_mode(true)
        .columnar(true)
        .build()
        .unwrap();
    println!("ready in {:.1}s, no restart", t.elapsed().as_secs_f64());

    let storage: Arc<Box<dyn StorageAdapter + Send + Sync>> =
        Arc::new(Box::new(LocalStorageAdapter::new(dir.join("storage"))));
    let engine = SyncEngine::new(Arc::clone(&db), storage);

    let c = db.collection("readings");
    for start in (0..200_000).step_by(10_000) {
        let docs: Vec<_> = (start..start + 10_000)
            .map(|i| {
                json!({"sensor": format!("sensor-{}", i % 12),
                            "celsius": (i % 400) as f64 / 10.0})
            })
            .collect();
        c.insert_many(&docs).unwrap();
    }
    engine.sync_table("readings", 200_001).unwrap();
    println!("synced; nobody asked for a columnar view\n");

    let bench = |label: &str, table: &str| {
        let sql = format!(
            "SELECT sensor, count(*) AS n, round(sum(celsius)::numeric, 6) AS s \
             FROM {table} GROUP BY sensor ORDER BY sensor"
        );
        let mut best = f64::MAX;
        let mut rows = Vec::new();
        for _ in 0..3 {
            let s = Instant::now();
            rows = db.query(&sql, &[]).unwrap();
            best = best.min(s.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{label:40} {best:8.1} ms");
        (best, rows)
    };

    let (pg, a) = bench("PostgreSQL, the collection", "readings");
    let (dk, b) = bench("DuckDB, readings_columnar", "readings_columnar");
    println!("  -> {:.2}x", pg / dk);
    assert_eq!(a, b, "the two paths must agree");
    println!("both paths agree over {} groups", a.len());

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
