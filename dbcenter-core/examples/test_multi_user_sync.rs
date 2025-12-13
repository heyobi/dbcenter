use dbcenter_core::DBCenter;
use dbcenter_core::storage::LocalStorageAdapter;
use dbcenter_core::sync_engine::SyncEngine;
use serde_json::json;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();
    println!("Starting Multi-User Sync Simulation (Offline A -> Cloud -> Online B)...");

    let db1_path = "./db1";
    let db2_path = "./db2";
    let storage_path = "./shared_storage";

    // Cleanup
    let _ = DBCenter::force_shutdown(db1_path);
    let _ = DBCenter::force_shutdown(db2_path);
    cleanup_dir(db1_path);
    cleanup_dir(db2_path);
    cleanup_dir(storage_path);
    std::fs::create_dir_all(storage_path)?;

    // ==========================================
    // USER A: Offline (Embedded Mode)
    // ==========================================
    println!("\n=== User A (Offline) ===");
    {
        println!("Initializing DB1 (daemon_mode=false)...");
        let db1 = DBCenter::builder()
            .path(db1_path)
            .persist(true)
            .daemon_mode(false) // Embedded behavior
            .multi_user_sync_mode(true)
            .build()?;

        let storage1 = Arc::new(Box::new(LocalStorageAdapter::new(PathBuf::from(storage_path))) as Box<dyn dbcenter_core::storage::StorageAdapter + Send + Sync>);
        let sync1 = SyncEngine::new(db1.clone(), storage1);
        let col1 = db1.collection("notes");

        println!("User A inserting 10 notes...");
        let mut docs = Vec::new();
        for i in 1..=10 {
            docs.push(json!({
                "title": format!("Note {}", i),
                "author": "User A",
                "content": "Offline content"
            }));
        }
        col1.insert_many(&docs)?;

        println!("User A syncing to storage...");
        // Sync items to storage (push)
        let count = sync1.sync_table("notes", 100)?;
        println!("User A synced {} items.", count);
        assert_eq!(count, 10);

        println!("User A shutting down...");
        db1.shutdown()?; // Clean shutdown
    }
    
    // Ensure DB1 is dead
    std::thread::sleep(Duration::from_secs(2));


    // ==========================================
    // USER B: Online (Service Mode)
    // ==========================================
    println!("\n=== User B (Online/Service) ===");
    {
        println!("Initializing DB2 (daemon_mode=true)...");
        // service=true implies it might be accessible via network, 
        // but for this test code running in same process, we use the struct instance.
        let db2 = DBCenter::builder()
            .path(db2_path)
            .persist(true)
            .daemon_mode(true) 
            .multi_user_sync_mode(true)
            .build()?;

        let storage2 = Arc::new(Box::new(LocalStorageAdapter::new(PathBuf::from(storage_path))) as Box<dyn dbcenter_core::storage::StorageAdapter + Send + Sync>);
        let sync2 = SyncEngine::new(db2.clone(), storage2);
        let col2 = db2.collection("notes");

        println!("User B initial state (should be empty):");
        let count_initial = col2.count(None)?;
        println!("Count: {}", count_initial);
        // It might be 0 because we are in a fresh DB2.
        
        println!("User B pulling from storage...");
        sync2.pull_and_apply("notes")?;

        println!("User B checking data...");
        let count_after = col2.count(None)?;
        println!("User B Note Count: {}", count_after);
        
        // Validation
        assert_eq!(count_after, 10, "User B should see 10 notes from User A");
        
        let sample = col2.find_one(None)?.expect("Should have data");
        println!("User B fetched sample: {:?}", sample);
        assert_eq!(sample["author"], "User A");

        println!("User B shutting down...");
        db2.shutdown()?;
    }

    println!("\nSUCCESS: Multi-User Sync Verified!");
    Ok(())
}

fn cleanup_dir(path: &str) {
    if std::path::Path::new(path).exists() {
        for _ in 0..5 {
            if std::fs::remove_dir_all(path).is_ok() { return; }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
