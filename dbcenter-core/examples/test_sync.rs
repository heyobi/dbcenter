use dbcenter_core::DBCenter;
use dbcenter_core::storage::LocalStorageAdapter;
use dbcenter_core::sync_engine::SyncEngine;
use serde_json::json;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    println!("Starting Sync Engine phase 3 test...");

    let test_db_path = "./sync_test_db";
    let storage_path = "./sync_storage";

    // Clean up previous run
    let _ = DBCenter::force_shutdown(test_db_path);
    if std::path::Path::new(test_db_path).exists() {
        std::fs::remove_dir_all(test_db_path)?;
    }
    if std::path::Path::new(storage_path).exists() {
        std::fs::remove_dir_all(storage_path)?;
    }
    std::fs::create_dir_all(storage_path)?;

    // 1. Setup DB (persist=true, service=false, multi_user_sync=true)
    let db = DBCenter::builder()
        .path(test_db_path)
        .persist(true)
        .daemon_mode(false)
        .multi_user_sync_mode(true)
        .build()?;

    println!("DB initialized.");

    // 2. Setup Storage & Sync Engine
    let storage = Arc::new(Box::new(LocalStorageAdapter::new(PathBuf::from(storage_path))) as Box<dyn dbcenter_core::storage::StorageAdapter + Send + Sync>);
    let sync_engine = SyncEngine::new(db.clone(), storage.clone());

    // 3. Create Collection & Insert 500 items
    let collection = db.collection("items");
    println!("Inserting 500 items...");
    
    // We insert 500 items. 
    // This will go into `items_adds`.
    let mut docs = Vec::new();
    for i in 0..500 {
        docs.push(json!({
            "name": format!("Item-{}", i),
            "value": i
        }));
    }
    // Batch insert for speed
    let ids = collection.insert_many(&docs)?;
    println!("Inserted {} items.", ids.len());

    // Verify _adds count
    let count_adds_sql = "SELECT COUNT(*) FROM items_adds";
    let res = db.query(count_adds_sql, &[])?;
    let count = res[0]["count"].as_i64().unwrap();
    println!("Items in _adds before sync: {}", count);
    assert_eq!(count, 500);

    // 4. Sync Process (Simulated)
    // Desired: "Add table should have only 2 values" after sync.
    // Meaning we sync 498 items and leave 2? Or we sync everything but keep 2 pending?
    // User request: "sonra add tablosunda sadece 2 tane değer olsun." (Afterwards add table should have only 2 values).
    // This implies we sync 498 items.
    // My SyncEngine::sync_table takes a limit.
    // Let's sync 498 items.
    
    println!("Syncing 498 items...");
    let synced_count = sync_engine.sync_table("items", 498)?;
    println!("Synced {} items.", synced_count);
    assert_eq!(synced_count, 498);

    // 5. Verify Result
    // Check storage has file
    let files = storage.list("items")?;
    println!("Storage files: {:?}", files);
    assert!(!files.is_empty(), "Storage should contain uploaded batches");

    // Check _adds count
    let res_after = db.query(count_adds_sql, &[])?;
    let count_after = res_after[0]["count"].as_i64().unwrap();
    println!("Items in _adds after sync: {}", count_after);
    assert_eq!(count_after, 2);

    // Check _base count (should have 498)
    let count_base_sql = "SELECT COUNT(*) FROM items_base";
    let res_base = db.query(count_base_sql, &[])?;
    let count_base = res_base[0]["count"].as_i64().unwrap();
    println!("Items in _base: {}", count_base);
    assert_eq!(count_base, 498);

    // Check View Count (should be 500 total)
    let view_count = collection.count(None)?;
    println!("Total Items in View: {}", view_count);
    assert_eq!(view_count, 500);

    println!("SUCCESS: Phase 3 Sync Engine verified!");

    db.shutdown()?;
    Ok(())
}
