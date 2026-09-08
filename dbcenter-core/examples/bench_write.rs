//! Does the delta layer write faster than a plain table?
//!
//! The idea is sound - writes land in a small hot table whose index stays in
//! memory, instead of scattering through a large one - but the current path puts
//! a per-row plpgsql trigger in front of it, and that is not free.

use dbcenter_core::DBCenter;
use std::time::Instant;

fn ms(d: std::time::Duration) -> String {
    format!("{:>8.1} ms", d.as_secs_f64() * 1000.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_rows = 2_000_000u64;
    let batch = 20_000u64;

    let path = std::env::temp_dir().join("dbcenter_bench_write");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::new(
        Some(path.to_str().unwrap()),
        true,
        false,
        None,
        None,
        None,
        None,
        true,
    )?;

    // A collection, so the delta tables and triggers exist.
    let _ = db.collection("events");

    // A plain table of the same shape, with a primary key, holding the same
    // amount of data the base layer will hold.
    db.query("CREATE TABLE plain (id uuid primary key, data jsonb, created_at timestamptz default now(), updated_at timestamptz default now())", &[])?;

    println!("temel katmana {base_rows} satır yükleniyor...");
    for t in ["plain", "events_base"] {
        db.query(&format!(
            "INSERT INTO {t} (id, data) SELECT gen_random_uuid(), jsonb_build_object('n', g) FROM generate_series(1,{base_rows}) g"), &[])?;
        db.query(&format!("ANALYZE {t}"), &[])?;
    }
    let size = |t: &str| -> String {
        db.query(
            &format!("SELECT pg_size_pretty(pg_total_relation_size('{t}')) AS s"),
            &[],
        )
        .map(|r| r[0]["s"].as_str().unwrap_or("?").to_string())
        .unwrap_or_default()
    };
    println!(
        "plain {} | events_base {}\n",
        size("plain"),
        size("events_base")
    );

    // Rows built on the client and sent over the wire, which is what any real
    // workload does. Generating them server-side with generate_series measures
    // something else entirely - no data crosses the connection - and makes the
    // API look slow for a reason that has nothing to do with the API.
    let values: Vec<String> = (0..batch)
        .map(|i| format!("(gen_random_uuid(), '{{\"n\": {i}}}'::jsonb)"))
        .collect();
    let sent = |table: &str| format!("INSERT INTO {table} (id, data) VALUES {}", values.join(","));

    println!("{batch} satır yazma (istemciden gönderilerek)");

    let t = Instant::now();
    db.query(&sent("plain"), &[])?;
    let plain = t.elapsed();
    println!("  büyük tabloya doğrudan      {}", ms(plain));

    // Through the delta view: INSTEAD OF trigger, one plpgsql call per row.
    let t = Instant::now();
    db.query(&sent("events"), &[])?;
    let delta = t.elapsed();
    println!(
        "  delta view (trigger)        {}   {:.2}x",
        ms(delta),
        delta.as_secs_f64() / plain.as_secs_f64()
    );

    // Straight into _adds, which is where the trigger would have put them.
    let t = Instant::now();
    db.query(&sent("events_adds"), &[])?;
    let adds = t.elapsed();
    println!(
        "  _adds'e doğrudan (triggersız) {}   {:.2}x",
        ms(adds),
        adds.as_secs_f64() / plain.as_secs_f64()
    );

    // And what the collection API now does, which is the same thing.
    let docs: Vec<serde_json::Value> = (0..batch).map(|i| serde_json::json!({"n": i})).collect();
    // The same statement insert_many builds: the document's own column, ids left
    // to the default, and the ids read back. Comparing against an insert that
    // supplies its own id and writes `data` measures different work.
    let c = db.collection("events");
    db.query(
        "ALTER TABLE events_adds ADD COLUMN IF NOT EXISTS n bigint",
        &[],
    )?;
    let equiv: Vec<String> = (0..batch).map(|i| format!("({i})")).collect();
    let t = Instant::now();
    for chunk in equiv.chunks(500) {
        db.query(
            &format!(
                "INSERT INTO events_adds (\"n\") VALUES {} RETURNING id::text",
                chunk.join(",")
            ),
            &[],
        )?;
    }
    let equivalent = t.elapsed();
    println!(
        "  aynı ifade, ham SQL         {}   {:.2}x",
        ms(equivalent),
        equivalent.as_secs_f64() / plain.as_secs_f64()
    );

    let t = Instant::now();
    c.insert_many(&docs)?;
    let api1 = t.elapsed();
    println!(
        "  insert_many (ilk çağrı)     {}   {:.2}x",
        ms(api1),
        api1.as_secs_f64() / plain.as_secs_f64()
    );
    let t = Instant::now();
    c.insert_many(&docs)?;
    let api2 = t.elapsed();
    println!(
        "  insert_many (ikinci)        {}   {:.2}x",
        ms(api2),
        api2.as_secs_f64() / plain.as_secs_f64()
    );

    println!(
        "\ntrigger'ın payı: {:.0}%",
        (1.0 - adds.as_secs_f64() / delta.as_secs_f64()) * 100.0
    );

    db.shutdown()?;
    let _ = std::fs::remove_dir_all(&path);
    Ok(())
}
