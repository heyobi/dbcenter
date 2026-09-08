//! Where the time goes, measured rather than guessed.
//!
//! Everything here runs against the same embedded PostgreSQL, so the difference
//! between the collection API and plain SQL is the API's own overhead, not the
//! database's.

use dbcenter_core::DBCenter;
use serde_json::json;
use std::time::Instant;

fn ms(d: std::time::Duration) -> String {
    format!("{:>8.1} ms", d.as_secs_f64() * 1000.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n = 2000;

    // --- startup -----------------------------------------------------------
    let path = std::env::temp_dir().join("dbcenter_bench");
    let _ = std::fs::remove_dir_all(&path);
    let t = Instant::now();
    let db = DBCenter::builder().path(&path).persist(true).build()?;
    println!("ilk açılış (initdb dahil)      {}", ms(t.elapsed()));
    db.shutdown()?;

    let t = Instant::now();
    let db = DBCenter::builder().path(&path).persist(true).build()?;
    println!("ikinci açılış (mevcut küme)    {}", ms(t.elapsed()));

    // --- writes ------------------------------------------------------------
    let docs: Vec<_> = (0..n)
        .map(|i| json!({"seq": i, "name": format!("row-{i}"), "flag": i % 2 == 0}))
        .collect();

    let c = db.collection("api");
    let t = Instant::now();
    c.insert_many(&docs)?;
    let api_insert = t.elapsed();
    println!("\n{n} satır yazma");
    println!("  collection.insert_many        {}", ms(api_insert));

    db.query(
        "CREATE TABLE raw (seq bigint, name text, flag boolean)",
        &[],
    )?;
    let t = Instant::now();
    for chunk in docs.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|d| {
                format!(
                    "({}, '{}', {})",
                    d["seq"],
                    d["name"].as_str().unwrap(),
                    d["flag"]
                )
            })
            .collect();
        db.query(&format!("INSERT INTO raw VALUES {}", values.join(",")), &[])?;
    }
    let raw_insert = t.elapsed();
    println!("  düz SQL INSERT                {}", ms(raw_insert));
    println!(
        "  API maliyeti                  {:.1}x",
        api_insert.as_secs_f64() / raw_insert.as_secs_f64()
    );

    // Per-call overhead: ensure_columns runs a catalog query every time.
    let t = Instant::now();
    for i in 0..100 {
        c.insert_one(&json!({"seq": 100000 + i, "name": "x", "flag": true}))?;
    }
    println!(
        "  insert_one x100               {}  ({:.2} ms/çağrı)",
        ms(t.elapsed()),
        t.elapsed().as_secs_f64() * 10.0
    );

    // --- reads -------------------------------------------------------------
    println!("\nokuma");
    let t = Instant::now();
    let found = c.find(None)?;
    println!(
        "  collection.find (tümü)        {}  ({} satır)",
        ms(t.elapsed()),
        found.len()
    );

    let t = Instant::now();
    let rows = db.query("SELECT * FROM api", &[])?;
    println!(
        "  düz SQL SELECT *              {}  ({} satır)",
        ms(t.elapsed()),
        rows.len()
    );

    let t = Instant::now();
    let _ = db.query("SELECT to_jsonb(t) FROM api t", &[])?;
    println!(
        "  SELECT to_jsonb(t)            {}   <- find() bunu yapıyor",
        ms(t.elapsed())
    );

    db.shutdown()?;
    let _ = std::fs::remove_dir_all(&path);

    // --- delta mode --------------------------------------------------------
    let dpath = std::env::temp_dir().join("dbcenter_bench_delta");
    let _ = std::fs::remove_dir_all(&dpath);
    let ddb = DBCenter::new(
        Some(dpath.to_str().unwrap()),
        true,
        false,
        None,
        None,
        None,
        None,
        true,
    )?;
    let dc = ddb.collection("api");
    dc.insert_many(&docs)?;
    println!("\ndelta modu (senkron için gereken)");
    let t = Instant::now();
    let d = dc.find(None)?;
    println!(
        "  find (view: UNION + NOT IN)   {}  ({} satır)",
        ms(t.elapsed()),
        d.len()
    );
    let t = Instant::now();
    let _ = ddb.query("SELECT * FROM api_adds", &[])?;
    println!("  doğrudan _adds tablosundan    {}", ms(t.elapsed()));
    ddb.shutdown()?;
    let _ = std::fs::remove_dir_all(&dpath);
    Ok(())
}
