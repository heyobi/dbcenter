//! Does the columnar path actually work, end to end, at DBCenter's own API?
//!
//! Writes documents through the collection API, syncs them to object storage,
//! then reads them back two ways from the same connection: the collection's
//! PostgreSQL view, and the DuckDB view over the Parquet.
use dbcenter_core::storage::StorageAdapter;
use dbcenter_core::sync_engine::SyncEngine;
use dbcenter_core::vfs::s3::{S3Config, S3StorageAdapter};
use dbcenter_core::DBCenter;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200_000);

    let dir = std::env::temp_dir().join("dbcenter_columnar_e2e");
    let _ = std::fs::remove_dir_all(&dir);

    let db = DBCenter::builder()
        .path(&dir)
        .persist(true)
        .multi_user_sync_mode(true)
        .build()
        .unwrap();

    let storage: Arc<Box<dyn StorageAdapter + Send + Sync>> = Arc::new(Box::new(
        S3StorageAdapter::with_prefix(
            S3Config {
                bucket: "bench".into(),
                region: "us-east-1".into(),
                endpoint: "http://127.0.0.1:19100".into(),
                access_key: "dbcenter".into(),
                secret_key: "dbcenter123".into(),
                session_token: None,
                path_style: true,
            },
            "e2e",
        )
        .unwrap(),
    ));
    // Object storage outlives the local database, so a previous run's files are
    // still under this prefix and the columnar view's glob would happily read
    // them. That is not a bug in the view - it is what a shared bucket means -
    // but it makes this comparison meaningless, so the prefix is cleared first.
    for key in storage.list("readings").unwrap() {
        storage.delete(&key).unwrap();
    }

    let engine = SyncEngine::new(Arc::clone(&db), Arc::clone(&storage));

    let collection = db.collection("readings");
    println!("writing {n} documents...");
    let start = Instant::now();
    for chunk in (0..n).collect::<Vec<_>>().chunks(10_000) {
        let docs: Vec<_> = chunk
            .iter()
            .map(|i| {
                json!({
                    "sensor": format!("sensor-{}", i % 12),
                    "celsius": (i % 400) as f64 / 10.0,
                    "count": (i % 1000) as i64,
                    "ok": i % 7 != 0,
                })
            })
            .collect();
        collection.insert_many(&docs).unwrap();
    }
    println!("  written in {:.1}s", start.elapsed().as_secs_f64());

    let start = Instant::now();
    let synced = engine.sync_table("readings", n + 1).unwrap();
    println!(
        "synced {synced} rows to object storage in {:.1}s",
        start.elapsed().as_secs_f64()
    );

    db.install_extension("pg_duckdb").unwrap();
    engine.ensure_columnar_view("readings").unwrap();
    println!("columnar view published\n");

    let bench = |label: &str, sql: &str| {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let s = Instant::now();
            let rows = db.query(sql, &[]).unwrap();
            best = best.min(s.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(rows);
        }
        println!("{label:46} {best:9.1} ms");
        best
    };

    let pg = bench(
        "PostgreSQL, the collection's own view",
        "SELECT sensor, count(*) AS n, sum(celsius) AS s FROM readings GROUP BY sensor ORDER BY sensor",
    );
    let dk = bench(
        "DuckDB, the same rows as Parquet",
        "SELECT sensor, count(*) AS n, sum(celsius) AS s FROM readings_columnar GROUP BY sensor ORDER BY sensor",
    );
    println!("  -> columnar is {:.2}x the speed of postgres\n", pg / dk);

    let a = db.query("SELECT sensor, count(*) AS n, round(sum(celsius)::numeric,3) AS s FROM readings GROUP BY sensor ORDER BY sensor", &[]).unwrap();
    let b = db.query("SELECT sensor, count(*) AS n, round(sum(celsius)::numeric,3) AS s FROM readings_columnar GROUP BY sensor ORDER BY sensor", &[]).unwrap();
    assert_eq!(a, b, "the two paths must return the same answer");
    println!(
        "both paths return identical results over {} groups",
        a.len()
    );

    // An update to an already-synced row is the case the view's deduplication
    // exists for: the row goes back into _adds and the next sync writes it to a
    // second delta, so the same id is now in two files. A view built when there
    // was one file does no deduplication, and would count that row twice.
    println!("\nupdating one row and syncing again...");
    let id: String = db
        .query("SELECT id::text AS id FROM readings LIMIT 1", &[])
        .unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    db.query(
        &format!("UPDATE readings SET sensor = 'sensor-updated' WHERE id = '{id}'::uuid"),
        &[],
    )
    .unwrap();
    engine.sync_table("readings", 1000).unwrap();

    let count = |sql: &str| -> i64 { db.query(sql, &[]).unwrap()[0]["n"].as_i64().unwrap() };
    let total_pg = count("SELECT count(*) AS n FROM readings");
    let total_col = count("SELECT count(*) AS n FROM readings_columnar");
    println!("  postgres {total_pg} rows, columnar {total_col} rows");
    assert_eq!(
        total_pg, total_col,
        "the updated row is in two parquet files; the columnar view must not count it twice"
    );

    let updated =
        count("SELECT count(*) AS n FROM readings_columnar WHERE sensor = 'sensor-updated'");
    assert_eq!(updated, 1, "the columnar view should show the new value");
    println!("  the update is visible and counted once");

    db.shutdown().unwrap();
}
