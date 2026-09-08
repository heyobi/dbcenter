//! Which PostgreSQL types survive the trip into JSON.
//!
//! Anything unmapped used to come back as null rather than an error, so a
//! column could go missing without a trace. NUMERIC was the worst of them: it
//! is what money and every exact decimal is stored as.

use dbcenter_core::DBCenter;
use serde_json::json;

fn test_db(name: &str) -> std::sync::Arc<DBCenter> {
    let path = std::env::temp_dir().join(format!("dbcenter_{name}"));
    let _ = std::fs::remove_dir_all(&path);
    DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .expect("embedded postgres should start")
}

#[test]
fn every_common_type_survives_the_trip_into_json() {
    let db = test_db("value_mapping");
    let one = |sql: &str| db.query(sql, &[]).unwrap().remove(0);

    // NUMERIC, in the shapes that actually turn up.
    assert_eq!(one("SELECT 19.99::numeric AS v")["v"], json!(19.99));
    assert_eq!(one("SELECT 0.5::numeric AS v")["v"], json!(0.5));
    assert_eq!(one("SELECT 0.0001::numeric AS v")["v"], json!(0.0001));
    assert_eq!(one("SELECT 12345.678::numeric AS v")["v"], json!(12345.678));
    assert_eq!(one("SELECT (-42.25)::numeric AS v")["v"], json!(-42.25));
    assert_eq!(one("SELECT 0::numeric AS v")["v"], json!(0.0));
    assert_eq!(one("SELECT 1000000::numeric AS v")["v"], json!(1000000.0));
    // Past what an f64 holds exactly, the digits are kept rather than rounded.
    assert_eq!(
        one("SELECT 123456789012345678901234567890.123::numeric AS v")["v"],
        json!("123456789012345678901234567890.123")
    );
    assert_eq!(one("SELECT NULL::numeric AS v")["v"], json!(null));

    // Dates and times.
    assert_eq!(
        one("SELECT '2026-09-08'::date AS v")["v"],
        json!("2026-09-08")
    );
    // ISO 8601 with a T, the same shape to_jsonb produces, so find() keeps
    // returning what callers already parse.
    assert_eq!(
        one("SELECT '2026-09-08 14:30:00'::timestamp AS v")["v"],
        json!("2026-09-08T14:30:00")
    );
    assert!(one("SELECT now() AS v")["v"]
        .as_str()
        .unwrap()
        .contains('T'));
    assert_eq!(one("SELECT '14:30:00'::time AS v")["v"], json!("14:30:00"));

    // Binary, rendered the way PostgreSQL prints it.
    assert_eq!(
        one(r"SELECT '\x0102ff'::bytea AS v")["v"],
        json!(r"\x0102ff")
    );

    // Arrays, including nulls inside them.
    assert_eq!(one("SELECT ARRAY[1,2,3] AS v")["v"], json!([1, 2, 3]));
    assert_eq!(
        one("SELECT ARRAY[1,NULL,3]::int[] AS v")["v"],
        json!([1, null, 3])
    );
    assert_eq!(
        one("SELECT ARRAY['a','b']::text[] AS v")["v"],
        json!(["a", "b"])
    );
    assert_eq!(
        one("SELECT ARRAY[true,false] AS v")["v"],
        json!([true, false])
    );
    assert_eq!(
        one("SELECT ARRAY[1.5,2.5]::float8[] AS v")["v"],
        json!([1.5, 2.5])
    );

    // The types that already worked must keep working.
    assert_eq!(one("SELECT 42 AS v")["v"], json!(42));
    assert_eq!(one("SELECT true AS v")["v"], json!(true));
    assert_eq!(one("SELECT 1.5::float8 AS v")["v"], json!(1.5));
    assert_eq!(one("SELECT 'hello' AS v")["v"], json!("hello"));
    assert_eq!(one("SELECT '{\"a\":1}'::jsonb AS v")["v"], json!({"a": 1}));
    assert!(
        one("SELECT gen_random_uuid() AS v")["v"]
            .as_str()
            .unwrap()
            .len()
            == 36
    );

    db.shutdown().unwrap();
}

#[test]
fn numeric_columns_round_trip_through_a_table() {
    // The literal path and the stored-column path go through different plans,
    // so exercise both.
    let db = test_db("numeric_roundtrip");
    db.query(
        "CREATE TABLE prices (id int, amount numeric(10,2), qty numeric)",
        &[],
    )
    .unwrap();
    db.query(
        "INSERT INTO prices VALUES (1, 19.99, 3), (2, 1234567.89, 0.001)",
        &[],
    )
    .unwrap();

    let rows = db
        .query("SELECT amount, qty FROM prices ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows[0]["amount"], json!(19.99));
    assert_eq!(rows[0]["qty"], json!(3.0));
    assert_eq!(rows[1]["amount"], json!(1234567.89));
    assert_eq!(rows[1]["qty"], json!(0.001));

    // And the aggregate a report would actually run.
    let total = db
        .query("SELECT sum(amount) AS t FROM prices", &[])
        .unwrap();
    assert_eq!(total[0]["t"], json!(1234587.88));

    db.shutdown().unwrap();
}
