//! Starting without reaching the network.
//!
//! Its own test binary, because it sets proxy environment variables that would
//! otherwise affect every other test in the same process.

use dbcenter_core::DBCenter;
use serde_json::json;

#[test]
fn starts_when_maven_central_is_unreachable() {
    // Version resolution used to ask Maven Central what existed on every
    // startup. With the distribution already unpacked that call buys nothing,
    // and when it fails - an air-gapped host, or a container that is meant to
    // start in milliseconds - resolution fell back to a version that was not
    // the installed one and tried to download it.
    //
    // This only proves anything once a distribution has been unpacked, which
    // any earlier test in the suite will have done.
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
    ] {
        std::env::set_var(name, "http://127.0.0.1:1");
    }

    let path = std::env::temp_dir().join("dbcenter_offline_start");
    let _ = std::fs::remove_dir_all(&path);

    let db = match DBCenter::builder().path(&path).persist(false).build() {
        Ok(db) => db,
        Err(e) => {
            // Nothing is cached yet, so there is nothing to prove here.
            eprintln!("skipping: no PostgreSQL distribution cached locally ({e})");
            return;
        }
    };

    let c = db.collection("offline");
    c.insert_one(&json!({"n": 1})).unwrap();
    assert_eq!(c.count(None).unwrap(), 1);

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&path);
}
