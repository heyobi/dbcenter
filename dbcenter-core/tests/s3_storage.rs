//! S3 storage adapter, exercised against a real S3 implementation.
//!
//! Runs only when `DBCENTER_S3_TEST_ENDPOINT` is set, so an ordinary
//! `cargo test` on a machine without object storage still passes. Bring one up
//! locally with:
//!
//! ```sh
//! docker run -d --name dbcenter-minio -p 19000:9000 \
//!   -e MINIO_ROOT_USER=dbcenter -e MINIO_ROOT_PASSWORD=dbcenter123 \
//!   minio/minio:latest server /data
//! ```
//!
//! then point the variables at it (see `scripts/s3-test-env.sh`).

use dbcenter_core::storage::StorageAdapter;
use dbcenter_core::vfs::s3::{S3Config, S3StorageAdapter};

/// Build an adapter from the test environment, or return None to skip.
fn adapter(prefix: &str) -> Option<S3StorageAdapter> {
    let endpoint = std::env::var("DBCENTER_S3_TEST_ENDPOINT").ok()?;
    let config = S3Config {
        bucket: std::env::var("DBCENTER_S3_TEST_BUCKET")
            .unwrap_or_else(|_| "dbcenter-test".to_string()),
        region: "us-east-1".to_string(),
        endpoint,
        access_key: std::env::var("DBCENTER_S3_TEST_KEY").unwrap_or_else(|_| "dbcenter".into()),
        secret_key: std::env::var("DBCENTER_S3_TEST_SECRET")
            .unwrap_or_else(|_| "dbcenter123".into()),
        session_token: None,
        path_style: true,
    };
    Some(S3StorageAdapter::with_prefix(config, prefix).expect("adapter"))
}

macro_rules! skip_without_s3 {
    ($prefix:expr) => {
        match adapter($prefix) {
            Some(a) => a,
            None => {
                eprintln!("skipping: DBCENTER_S3_TEST_ENDPOINT is not set");
                return;
            }
        }
    };
}

#[test]
fn round_trips_objects() {
    let s3 = skip_without_s3!("roundtrip");

    let payload = b"the quick brown fox jumps over the lazy dog".to_vec();
    s3.upload("tables/users.parquet", &payload).unwrap();

    assert_eq!(s3.download("tables/users.parquet").unwrap(), payload);
    assert_eq!(
        s3.size("tables/users.parquet").unwrap(),
        payload.len() as u64
    );

    // Ranged reads are what the Parquet VFS relies on: it fetches the footer
    // and then only the row groups it needs.
    assert_eq!(
        s3.download_range("tables/users.parquet", 4, 5).unwrap(),
        b"quick"
    );
    assert_eq!(
        s3.download_range("tables/users.parquet", 40, 3).unwrap(),
        b"dog"
    );
    assert!(s3
        .download_range("tables/users.parquet", 0, 0)
        .unwrap()
        .is_empty());

    s3.delete("tables/users.parquet").unwrap();
    assert!(s3.download("tables/users.parquet").is_err());
    // Deleting again is not an error.
    s3.delete("tables/users.parquet").unwrap();
}

#[test]
fn lists_by_prefix_and_strips_the_namespace() {
    let s3 = skip_without_s3!("listing");

    for key in ["a/one.txt", "a/two.txt", "b/three.txt"] {
        s3.upload(key, key.as_bytes()).unwrap();
    }

    let mut found = s3.list("a/").unwrap();
    found.sort();
    assert_eq!(
        found,
        vec!["a/one.txt".to_string(), "a/two.txt".to_string()]
    );

    let mut all = s3.list("").unwrap();
    all.sort();
    assert_eq!(
        all.len(),
        3,
        "prefix must be stripped from returned keys: {all:?}"
    );

    for key in ["a/one.txt", "a/two.txt", "b/three.txt"] {
        s3.delete(key).unwrap();
    }
}

#[test]
fn keys_with_spaces_and_unicode_survive_signing() {
    let s3 = skip_without_s3!("encoding");

    // The signature covers the encoded path, so any mismatch between what is
    // signed and what is sent shows up here as a 403.
    let key = "tables/şehir listesi/veri 2026.parquet";
    s3.upload(key, b"ok").unwrap();
    assert_eq!(s3.download(key).unwrap(), b"ok");
    s3.delete(key).unwrap();
}

#[test]
fn rename_copies_then_removes() {
    let s3 = skip_without_s3!("rename");

    s3.upload("staging/part.tmp", b"payload").unwrap();
    s3.rename("staging/part.tmp", "final/part.parquet").unwrap();

    assert_eq!(s3.download("final/part.parquet").unwrap(), b"payload");
    assert!(s3.download("staging/part.tmp").is_err());

    s3.delete("final/part.parquet").unwrap();
}

#[test]
fn only_one_worker_holds_a_lock() {
    let s3 = skip_without_s3!("locking");
    let key = "locks/vacuum.lock";
    let _ = s3.delete(key);

    assert!(s3.lock(key, 60_000, "worker-a").unwrap());
    assert!(
        !s3.lock(key, 60_000, "worker-b").unwrap(),
        "a live lease must not be handed to a second worker"
    );

    // Only the holder can extend or release it.
    assert!(!s3.refresh_lock(key, 60_000, "worker-b").unwrap());
    assert!(s3.refresh_lock(key, 60_000, "worker-a").unwrap());

    s3.unlock(key, "worker-b").unwrap();
    assert!(
        !s3.lock(key, 60_000, "worker-b").unwrap(),
        "unlock by a non-holder must not release the lease"
    );

    s3.unlock(key, "worker-a").unwrap();
    assert!(s3.lock(key, 60_000, "worker-b").unwrap());
    s3.unlock(key, "worker-b").unwrap();
}

#[test]
fn an_expired_lease_can_be_taken_over() {
    let s3 = skip_without_s3!("expiry");
    let key = "locks/stale.lock";
    let _ = s3.delete(key);

    // A machine that died mid-sync leaves its lock behind. Once the lease has
    // run out another machine has to be able to pick the work up, otherwise a
    // crash wedges the whole cluster.
    assert!(s3.lock(key, 1, "crashed-worker").unwrap());
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(s3.lock(key, 60_000, "fresh-worker").unwrap());

    s3.unlock(key, "fresh-worker").unwrap();
}
