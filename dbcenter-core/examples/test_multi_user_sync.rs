use dbcenter_core::DBCenter;
use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use std::sync::Arc;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let storage_dir = PathBuf::from("storage_emu");
    if storage_dir.exists() {
         let _ = std::fs::remove_dir_all(&storage_dir);
    }
    std::fs::create_dir_all(&storage_dir)?;
    
    // Shared Storage (Cloud)
    // We reuse this adapter concept, but since we want to drop everything cleanly,
    // let's create separate instances or clone Arc inside scopes.
    // Actually, local storage adapter just points to path. Safe to create new ones.
    
    // Explicit Cleanup of DBs
    let db1_path = PathBuf::from("db1");
    if db1_path.exists() {
        println!("Cleaning up old db1...");
        let _ = DBCenter::force_shutdown(&db1_path);
        let _ = std::fs::remove_dir_all(&db1_path);
    }
    
    let db2_path = PathBuf::from("db2");
    if db2_path.exists() {
        println!("Cleaning up old db2...");
        let _ = DBCenter::force_shutdown(&db2_path);
        let _ = std::fs::remove_dir_all(&db2_path);
    }

    // --- User A Scope ---
    {
        println!("=== User A (Offline) ===");
        let storage_t1: Arc<Box<dyn StorageAdapter + Send + Sync>> = Arc::new(Box::new(LocalStorageAdapter::new(storage_dir.clone())));
        
        let db1 = DBCenter::builder()
            .path(&db1_path)
            .persist(true)
            .daemon_mode(false)
            .multi_user_sync_mode(true)
            .build()?;

        let col1 = db1.collection("notes");
        
        println!("User A inserting 10 records...");
        for i in 1..=10 {
            col1.insert_one(&json!({
                "title": format!("Note {}", i),
                "content": "Offline content",
                "author": "User A"
            }))?;
        }
        
        println!("User A syncing to storage...");
        let sync1 = SyncEngine::new(db1.clone(), storage_t1);
        let synced_count = sync1.sync_table("notes", 100)?; 
        println!("User A Synced {} records.", synced_count);
        
        println!("User A flushing/dropping...");
        // sync1 drops here
        // db1 drops here -> Auto shutdown because daemon_mode=false
    }
    
    println!("User A Scope End. Waiting 3s for full cleanup...");
    std::thread::sleep(std::time::Duration::from_secs(3));

    // --- User B Scope ---
    {
        println!("\n=== User B (Online/Service) ===");
        let storage_t2: Arc<Box<dyn StorageAdapter + Send + Sync>> = Arc::new(Box::new(LocalStorageAdapter::new(storage_dir.clone())));
        
        // Use daemon_mode=false to ensure process exits cleanly with test
        let db2 = DBCenter::builder()
            .path(&db2_path)
            .persist(true)
            .daemon_mode(false) 
            .multi_user_sync_mode(true)
            .build()?;

        let col2 = db2.collection("notes");
        let sync2 = SyncEngine::new(db2.clone(), storage_t2.clone());
        
        println!("User B Initialized. Collection obtained.");

        let count_before = col2.find(None)?.len();
        println!("User B initial state (should be empty):");
        println!("Count: {}", count_before);

        // Pull from Shared Storage
        println!("User B pulling from storage (VFS & Conflict Resolution)...");
        match sync2.pull_and_apply("notes", SyncMode::Full) {
            Ok(_) => println!("User B Apply Success."),
            Err(e) => println!("User B Apply Failed: {:?}", e),
        }
        
        println!("User B checking data...");
        let all_notes = col2.find(None)?;
        println!("User B Note Count: {}", all_notes.len());
        
        assert_eq!(all_notes.len(), 10, "User B should have 10 notes");

        // --- Vacuum / Compaction Test ---
        println!("User B Performing Vacuum (Compaction)...");
        // Currently storage should contain DELTA files
        let files_before = storage_t2.list("notes")?;
        println!("Files before vacuum: {:?}", files_before);
        assert!(files_before.iter().any(|f| f.contains("_DELTA_")), "Should have DELTA files");

        match sync2.vacuum("notes") {
            Ok(_) => println!("Vacuum Success."),
            Err(e) => {
                 println!("Vacuum Failed: {:?}", e);
                 return Err(Box::new(e));
            }
        }
        
        let files_after = storage_t2.list("notes")?;
        println!("Files after vacuum: {:?}", files_after);
        assert!(files_after.iter().any(|f| f.contains("_BASE_")), "Should have BASE file");
        assert!(!files_after.iter().any(|f| f.contains("_DELTA_")), "Should NOT have DELTA files");

        // Verify Data Integrity after Vacuum
        let all_notes_after = col2.find(None)?;
        println!("User B Note Count Post-Vacuum: {}", all_notes_after.len());
        assert_eq!(all_notes_after.len(), 10, "Data should be preserved after vacuum");
        
        println!("User B Test Complete. Dropping...");
    } // db2 drops -> Shutdown

    println!("Test Ending successfully.");
    Ok(())
}
