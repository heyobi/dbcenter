//! Regression tests for the collection query layer.
//!
//! Each case here corresponds to a bug where a malformed or hostile query
//! silently did the wrong thing against a real PostgreSQL instance, so these
//! run against an embedded server rather than asserting on generated SQL.

use dbcenter_core::DBCenter;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn q(pairs: Vec<(&str, Value)>) -> HashMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn test_db() -> Arc<DBCenter> {
    let path = std::env::temp_dir().join("dbcenter_collection_regression");
    let _ = std::fs::remove_dir_all(&path);
    DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .expect("embedded postgres should start")
}

#[test]
fn collection_query_regressions() {
    let db = test_db();

    // An unrecognised operator used to contribute no condition at all, which
    // turned a targeted delete into "delete everything".
    let c = db.collection("unknown_op");
    c.insert_many(&[
        json!({"name": "a", "age": 10}),
        json!({"name": "b", "age": 20}),
    ])
    .unwrap();
    let err = c
        .delete_many(Some(&q(vec![("age", json!({"$bogus": 999}))])))
        .expect_err("unknown operator must be rejected");
    assert!(
        err.to_string().contains("Unsupported query operator"),
        "{err}"
    );
    assert_eq!(
        c.count(None).unwrap(),
        2,
        "rows must survive a rejected query"
    );

    // Same failure shape: $in with a non-array dropped the condition.
    let err = c
        .delete_many(Some(&q(vec![("name", json!({"$in": "a"}))])))
        .expect_err("$in with a scalar must be rejected");
    assert!(err.to_string().contains("requires an array"), "{err}");
    assert_eq!(c.count(None).unwrap(), 2);

    // $in still works when given an array.
    assert_eq!(
        c.delete_many(Some(&q(vec![("name", json!({"$in": ["a"]}))])))
            .unwrap(),
        1
    );
    assert_eq!(c.count(None).unwrap(), 1);

    // An empty query means "every row"; it used to splice a bare `WHERE` and
    // fail with a syntax error.
    let c = db.collection("empty_query");
    c.insert_many(&[
        json!({"name": "a", "age": 1}),
        json!({"name": "b", "age": 1}),
    ])
    .unwrap();
    assert_eq!(
        c.update_many(&q(vec![]), &q(vec![("$set", json!({"age": 2}))]))
            .unwrap(),
        2
    );
    for doc in c.find(None).unwrap() {
        assert_eq!(doc["age"], json!(2));
    }

    // A field name carrying a double quote must stay inside one quoted
    // identifier instead of terminating it and injecting SQL.
    let c = db.collection("injection");
    c.insert_many(&[json!({"name": "keep"})]).unwrap();
    let hostile = r#"name" = 'keep' OR "1"="1"#;
    let result = c.delete_many(Some(&q(vec![(hostile, json!("zzz"))])));
    assert!(
        result.is_err(),
        "a hostile field name should resolve to an unknown column, not a live predicate"
    );
    assert_eq!(c.count(None).unwrap(), 1, "the row must not be deleted");

    // Values containing quotes round-trip rather than breaking the literal.
    let c = db.collection("quoting");
    c.insert_one(&json!({"name": "O'Brien"})).unwrap();
    assert_eq!(
        c.count(Some(&q(vec![("name", json!("O'Brien"))]))).unwrap(),
        1
    );

    // "data" is only reserved in delta mode; in the relational layout it is an
    // ordinary field, and used to be dropped at schema-evolution time so the
    // insert failed against a column that was never created.
    let c = db.collection("data_field");
    c.insert_one(&json!({"name": "x", "data": "payload"}))
        .unwrap();
    assert_eq!(c.find(None).unwrap()[0]["data"], json!("payload"));

    // insert_many must reject a non-object document instead of panicking.
    let c = db.collection("bad_doc");
    assert!(c.insert_many(&[json!("not an object")]).is_err());

    db.shutdown().unwrap();
}

#[test]
fn fuzzy_search_works_on_the_relational_layout() {
    let path = std::env::temp_dir().join("dbcenter_fuzzy_regression");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .expect("embedded postgres should start");

    // pg_trgm could not load at all while dynamic_library_path was joined with
    // the Windows path separator, and fuzzy_search then addressed a `data`
    // JSONB column that a relational collection does not have.
    let c = db.collection("cities");
    c.insert_many(&[
        json!({"name": "Istanbul"}),
        json!({"name": "Ankara"}),
        json!({"name": "Izmir"}),
    ])
    .unwrap();

    let hits = c.fuzzy_search("Istanbu", "name", None, None).unwrap();
    assert!(!hits.is_empty(), "expected a fuzzy match for 'Istanbu'");
    assert_eq!(hits[0]["name"], json!("Istanbul"));
    assert!(hits[0]["similarity"].as_f64().unwrap() > 0.5);

    assert!(c.similarity("abc", "abcdef").unwrap() > 0.0);

    // An out-of-range threshold is a caller error, not something to clamp.
    assert!(c.fuzzy_search("x", "name", None, Some(5.0)).is_err());
    assert!(c.fuzzy_search("x", "name", Some("nonsense"), None).is_err());

    // create_index has to target the same expression fuzzy_search does.
    c.create_index("name", "btree").unwrap();
    assert!(c.create_index("name", "not_an_index_method").is_err());

    db.shutdown().unwrap();
}

#[test]
#[cfg(unix)]
fn starts_from_a_data_directory_too_deep_for_a_unix_socket() {
    // sockaddr_un.sun_path holds 108 bytes, so a data directory much more than
    // sixty characters deep used to make PostgreSQL fail at startup with an
    // error about the socket rather than the path. Deep paths are ordinary in
    // CI workspaces and containers.
    let deep = std::env::temp_dir()
        .join("dbcenter_deep")
        .join("a".repeat(40))
        .join("b".repeat(40))
        .join("c".repeat(40));
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("dbcenter_deep"));

    assert!(
        deep.join("sockets/.s.PGSQL.5432").as_os_str().len() > 107,
        "the test path must actually exceed the limit"
    );

    let db = DBCenter::builder()
        .path(&deep)
        .persist(false)
        .build()
        .expect("a deep data directory must not prevent startup");

    let c = db.collection("deep");
    c.insert_one(&json!({"ok": 1})).unwrap();
    assert_eq!(c.count(None).unwrap(), 1);

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("dbcenter_deep"));
}

#[test]
fn a_password_containing_a_quote_is_set_literally() {
    // ALTER USER takes no bind parameters, so the password was interpolated
    // straight into SQL. An apostrophe in it either broke the statement or ran
    // as SQL - and this path is fed straight from an environment variable when
    // the engine runs in a container.
    let path = std::env::temp_dir().join("dbcenter_password_quoting");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .unwrap();

    let awkward = "p'a\"ss--word;";
    db.set_user_password("postgres", awkward)
        .expect("an awkward password must still be accepted");

    // The role must still exist and the server must still be usable, which
    // would not be true if the trailing fragment had executed as SQL.
    let c = db.collection("after_password");
    c.insert_one(&json!({"ok": 1})).unwrap();
    assert_eq!(c.count(None).unwrap(), 1);

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&path);
}

#[test]
fn a_field_introduced_later_still_gets_a_column() {
    // The set of columns a table has is cached, because asking the catalogue on
    // every insert cost more than the insert. This is the case that cache could
    // break: a document with a field nothing has used before, after the cache
    // has been filled.
    let path = std::env::temp_dir().join("dbcenter_schema_cache");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .unwrap();

    let c = db.collection("people");
    c.insert_one(&json!({"name": "Ada"})).unwrap();
    c.insert_one(&json!({"name": "Grace"})).unwrap(); // cache is warm now

    c.insert_one(&json!({"name": "Katherine", "role": "mathematician"}))
        .unwrap();
    c.insert_one(&json!({"name": "Joan", "role": "engineer", "year": 1936}))
        .unwrap();

    let mut docs = c.find(None).unwrap();
    docs.sort_by_key(|d| d["name"].as_str().unwrap().to_string());
    assert_eq!(docs.len(), 4);
    assert_eq!(docs[0]["name"], json!("Ada"));
    assert_eq!(
        docs[0]["role"],
        json!(null),
        "the earlier rows have no role"
    );
    // Sorted by name: Ada, Grace, Joan, Katherine.
    assert_eq!(docs[2]["role"], json!("engineer"), "{:?}", docs[2]);
    assert_eq!(docs[2]["year"], json!(1936), "{:?}", docs[2]);
    assert_eq!(docs[3]["role"], json!("mathematician"), "{:?}", docs[3]);

    // And querying by the new column works, so it is a real column rather than
    // something that only looks right on the way out.
    assert_eq!(
        c.count(Some(&q(vec![("role", json!("engineer"))])))
            .unwrap(),
        1
    );

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&path);
}
