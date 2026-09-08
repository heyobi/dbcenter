//! What happens to the API when several threads use it at once?
//!
//! pgbench opens a connection per client and scales with the cores. The library
//! holds one connection behind a mutex, so every caller queues behind every
//! other one no matter how many threads there are.

use dbcenter_core::DBCenter;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join("dbcenter_bench_conc");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::builder().path(&path).persist(false).build()?;

    db.query("CREATE TABLE t (id serial primary key, n bigint)", &[])?;
    db.query(
        "INSERT INTO t (n) SELECT g FROM generate_series(1,100000) g",
        &[],
    )?;
    db.query("ANALYZE t", &[])?;

    let seconds = 5;
    println!("{seconds} saniye, salt okuma (SELECT ... WHERE id = ?)\n");
    println!(
        "{:<10} {:<14} {:<14} ölçekleme",
        "iş parçacığı", "sorgu/sn", "gecikme(ms)"
    );

    let mut baseline = 0.0;
    for threads in [1usize, 2, 4, 8] {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let counters: Vec<Arc<std::sync::atomic::AtomicU64>> = (0..threads)
            .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0)))
            .collect();

        let handles: Vec<_> = counters
            .iter()
            .enumerate()
            .map(|(i, counter)| {
                let db = Arc::clone(&db);
                let stop = Arc::clone(&stop);
                let counter = Arc::clone(counter);
                std::thread::spawn(move || {
                    let mut k = (i * 7919) as i64 % 100_000;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        k = (k + 7919) % 100_000;
                        let _ = db.query(&format!("SELECT n FROM t WHERE id = {}", k + 1), &[]);
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                })
            })
            .collect();

        let t = Instant::now();
        std::thread::sleep(Duration::from_secs(seconds));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for h in handles {
            let _ = h.join();
        }
        let elapsed = t.elapsed().as_secs_f64();

        let total: u64 = counters
            .iter()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .sum();
        let qps = total as f64 / elapsed;
        if threads == 1 {
            baseline = qps;
        }
        println!(
            "{:<10} {:<14.0} {:<14.3} {:.2}x",
            threads,
            qps,
            1000.0 * threads as f64 / qps,
            qps / baseline
        );
    }

    db.shutdown()?;
    let _ = std::fs::remove_dir_all(&path);
    Ok(())
}
