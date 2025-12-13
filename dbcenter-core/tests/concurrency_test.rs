
use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter, LockFile};
use dbcenter_core::sync_engine::SyncEngine;
use dbcenter_core::DBCenter;
use std::sync::Arc;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::thread;
use std::fs;
use uuid::Uuid;

fn setup(name: &str) -> (Arc<DBCenter>, Arc<Box<dyn StorageAdapter + Send + Sync>>, PathBuf) {
    let test_dir = std::env::temp_dir().join(format!("dbcenter_test_{}", name));
    let db_path = test_dir.join("db");
    let storage_path = test_dir.join("storage");
    
    if test_dir.exists() {
        fs::remove_dir_all(&test_dir).unwrap();
    }
    fs::create_dir_all(&storage_path).unwrap();

    let db = DBCenter::new(
        Some(db_path.to_str().unwrap()), 
        true, 
        false, 
        None, None, None, None, // Defaults
        true // Multi-user sync enabled
    ).unwrap();
    
    let storage = Arc::new(Box::new(LocalStorageAdapter::new(storage_path.clone())) as Box<dyn StorageAdapter + Send + Sync>);
    
    (db, storage, storage_path)
}

#[test]
fn test_lock_contention() {
    let (db, storage, storage_path) = setup("contention");
    let table_name = "contention_table";
    
    // Insert data to prevent "vacuum empty" optimization (though implementation logs and continues)
    // We need to use DBCenter API to insert data.
    // SyncEngine::vacuum requires _base table. 
    // Pull_and_apply ensures it exists.
    // But we need actual data to be interesting?
    // Actually, vacuum works on empty tables too.
    
    // 1. Acquire Lock Manually (simulate User A)
    let lock_key = format!("{}_vacuum.lock", table_name);
    let worker_a = Uuid::new_v4().to_string();
    let acquired = storage.lock(&lock_key, 5000, &worker_a).unwrap();
    assert!(acquired, "User A failed to acquire lock");
    
    // 2. Spawn Thread for Vacuum (User B)
    let db_clone = db.clone();
    let storage_clone = storage.clone();
    
    let handle = thread::spawn(move || {
        let sync = SyncEngine::new(db_clone, storage_clone);
        let start = SystemTime::now();
        println!("User B starting vacuum...");
        let res = sync.vacuum(table_name);
        let elapsed = start.elapsed().unwrap();
        (res, elapsed)
    });
    
    // 3. Wait 2 seconds then Unlock
    thread::sleep(Duration::from_secs(2));
    storage.unlock(&lock_key, &worker_a).unwrap();
    println!("User A released lock");
    
    // 4. Join and Verify
    let (res, elapsed) = handle.join().unwrap();
    assert!(res.is_ok(), "Vacuum failed: {:?}", res.err());
    assert!(elapsed.as_secs() >= 2, "Vacuum finished too fast ({:?}), should have waited for lock", elapsed);
    println!("Vacuum success in {:?}", elapsed);
}

#[test]
fn test_stale_lock_recovery() {
    let (db, storage, storage_path) = setup("stale_recovery");
    let table_name = "stale_table";
    let lock_key = format!("{}_vacuum.lock", table_name);

    // 1. Create Stale Lock
    let stale_lock = LockFile {
        expires_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 - 10000,
        worker_id: "dead_worker".to_string(),
    };
    let json = serde_json::to_string(&stale_lock).unwrap();
    fs::write(storage_path.join(&lock_key), json).unwrap();
    
    // 2. Run Vacuum
    let sync = SyncEngine::new(db, storage);
    let start = SystemTime::now();
    let res = sync.vacuum(table_name);
    let elapsed = start.elapsed().unwrap();
    
    assert!(res.is_ok(), "Vacuum failed on stale lock recovery");
    assert!(elapsed.as_secs() < 3, "Vacuum took too long ({:?}), should recover immediately", elapsed);
    
    // Verify lock file is gone (vacuum cleanup) or replaced (and then deleted)
    assert!(!storage_path.join(&lock_key).exists(), "Lock file should be cleaned up");
}
