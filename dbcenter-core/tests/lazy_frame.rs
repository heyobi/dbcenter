//! LazyFrame, which had no test of any kind.
//!
//! It is a public, advertised feature - the Polars-shaped query builder - and
//! nothing exercised it, in Rust or Python.

use dbcenter_core::lazy_frame::LazyFrame;
use dbcenter_core::DBCenter;
use serde_json::json;
use std::sync::Arc;

fn db(name: &str) -> std::sync::Arc<DBCenter> {
    let path = std::env::temp_dir().join(format!("dbcenter_lazy_frame_{name}"));
    let _ = std::fs::remove_dir_all(&path);
    DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .unwrap()
}

#[test]
fn the_builder_composes() {
    let db = db("compose");
    let c = db.collection("people");
    c.insert_many(&[
        json!({"name": "Ada",     "age": 36, "city": "London"}),
        json!({"name": "Grace",   "age": 45, "city": "New York"}),
        json!({"name": "Katherine", "age": 52, "city": "Hampton"}),
        json!({"name": "Joan",    "age": 29, "city": "London"}),
    ])
    .unwrap();

    let all = LazyFrame::new(Arc::clone(&db), "people").collect().unwrap();
    assert_eq!(all.len(), 4);

    // filter
    let older = LazyFrame::new(Arc::clone(&db), "people")
        .filter("age > 40".into())
        .collect()
        .unwrap();
    assert_eq!(older.len(), 2, "{older:?}");

    // filters compose as AND
    let both = LazyFrame::new(Arc::clone(&db), "people")
        .filter("age > 30".into())
        .filter("city = 'London'".into())
        .collect()
        .unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0]["name"], json!("Ada"));

    // select narrows the columns
    let names = LazyFrame::new(Arc::clone(&db), "people")
        .select(vec!["name".into()])
        .collect()
        .unwrap();
    assert_eq!(names.len(), 4);
    assert_eq!(
        names[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["name"],
        "select should return only the named columns"
    );

    // order_by, limit, offset
    let sorted = LazyFrame::new(Arc::clone(&db), "people")
        .order_by("age ASC".into())
        .collect()
        .unwrap();
    let ages: Vec<i64> = sorted.iter().map(|r| r["age"].as_i64().unwrap()).collect();
    assert_eq!(ages, vec![29, 36, 45, 52]);

    let page = LazyFrame::new(Arc::clone(&db), "people")
        .order_by("age ASC".into())
        .limit(2)
        .offset(1)
        .collect()
        .unwrap();
    let ages: Vec<i64> = page.iter().map(|r| r["age"].as_i64().unwrap()).collect();
    assert_eq!(ages, vec![36, 45], "limit and offset must both apply");

    db.shutdown().unwrap();
}

#[test]
fn arrow_export_matches_the_rows() {
    let db = db("arrow");
    let c = db.collection("readings");
    c.insert_many(&[
        json!({"sensor": "a", "value": 1.5, "ok": true}),
        json!({"sensor": "b", "value": 2.5, "ok": false}),
    ])
    .unwrap();

    let ipc = LazyFrame::new(Arc::clone(&db), "readings")
        .to_arrow_ipc()
        .unwrap();
    assert!(!ipc.is_empty(), "an export of two rows should not be empty");
    // Arrow IPC streams start with a continuation marker.
    assert_eq!(
        &ipc[..4],
        &[0xff, 0xff, 0xff, 0xff],
        "not an Arrow IPC stream"
    );

    // An empty result is an empty buffer rather than an error.
    let empty = LazyFrame::new(Arc::clone(&db), "readings")
        .filter("sensor = 'nobody'".into())
        .to_arrow_ipc()
        .unwrap();
    assert!(empty.is_empty());

    db.shutdown().unwrap();
}

#[test]
fn a_bad_filter_is_an_error_not_an_empty_result() {
    // filter() takes raw SQL. A caller who gets it wrong should be told, not
    // handed zero rows that look like "nothing matched".
    let db = db("bad_filter");
    db.collection("t").insert_one(&json!({"a": 1})).unwrap();

    let err = LazyFrame::new(Arc::clone(&db), "t")
        .filter("this is not sql".into())
        .collect()
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("syntax")
            || err.to_string().to_lowercase().contains("column"),
        "unhelpful error: {err}"
    );

    db.shutdown().unwrap();
}
