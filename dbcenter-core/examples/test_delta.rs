use dbcenter_core::DBCenter;
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    println!("Starting Delta Layer simulation test...");

    let test_dir = "./delta_test_db";
    // Ensure standard cleanup
    let _ = DBCenter::force_shutdown(test_dir); 
    
    if std::path::Path::new(test_dir).exists() {
        // Retry loop for windows file locking
        for _ in 0..5 {
            if std::fs::remove_dir_all(test_dir).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // 1. Start DBCenter with multi_user_sync_mode = true
    let db = DBCenter::builder()
        .path(test_dir)
        .persist(true) // Simulate persistent storage (simulating drive)
        .daemon_mode(false) // No background service, just embedded access
        .multi_user_sync_mode(true) // ENABLE PHASE 2
        .build()?;

    println!("DBCenter started at {:?}", db.get_path());

    // 2. Create a collection "products"
    // This should trigger DeltaManager to create products_base, products_adds, products_dels
    let collection = db.collection("products");
    
    // We insert a document to ensure the table creation logic has definitely run 
    // (though new() calls ensure_table, so it should be there)
    
    // 3. Verify Physical Tables Existence
    let tables = db.list_collections()?;
    println!("Physical tables found: {:?}", tables);
    
    assert!(tables.contains(&"products_base".to_string()), "products_base missing");
    assert!(tables.contains(&"products_adds".to_string()), "products_adds missing");
    assert!(tables.contains(&"products_dels".to_string()), "products_dels missing");
    
    // "products" should NOT be in tables list because list_collections only shows pg_tables (physicals)
    // but we can query it to prove it exists as a View.

    // 4. Insert Data (User A)
    println!("Inserting data into View 'products'...");
    let id1 = collection.insert_one(&json!({"name": "Laptop", "price": 1000}))?;
    // Note: With UUIDs, return type of insert_one might change or need handling.
    // Currently Collection::insert_one returns Result<i32>. We need to update that too if we want UUIDs returned.
    // For now, let's assume insert_one still returns i32 (which is wrong for UUIDs) or we updated it.
    // Actually, we haven't updated Collection::insert_one return type yet.
    // However, the TRIGGER returns the ID.
    // Let's modify collection.rs to return String (or Value) for ID.
    // But first, let's adjust this test assuming insert_one returns something we can print.
    let id2 = collection.insert_one(&json!({"name": "Mouse", "price": 50}))?;
    println!("Inserted IDs: {}, {}", id1, id2);

    // Verify view has data
    let items = collection.find(None)?;
    println!("Items in View: {:?}", items);
    assert_eq!(items.len(), 2);
    
    // Verify physical placement
    // Data should be in products_adds
    // Use raw query to avoid triggering 'ensure_table' -> 'ensure_delta_tables' on the adds table itself
    let sql_adds = "SELECT data FROM products_adds";
    let adds_rows = db.query(sql_adds, &[])?;
    println!("Rows in x_adds: {:?}", adds_rows);
    
    // Check if any row has name=Laptop. Note: db.query returns rows where columns are fields.
    // 'data' column is a JSONB value.
    let has_laptop = adds_rows.iter().any(|row| {
        // row is a serde_json::Value Object map
        if let Some(data_val) = row.get("data") {
            data_val["name"] == "Laptop"
        } else {
            false
        }
    });
    assert!(has_laptop, "Laptop should be in products_adds");

    // 5. Update Data
    println!("Updating Laptop price to 1200...");
    let query = std::collections::HashMap::from([("name".to_string(), json!("Laptop"))]);
    let update = std::collections::HashMap::from([("price".to_string(), json!(1200))]);
    collection.update_one(&query, &update)?;

    // Verify View
    let items_after = collection.find(None)?;
    let laptop = items_after.iter().find(|i| i["name"] == "Laptop").unwrap();
    assert_eq!(laptop["price"], 1200);
    println!("View shows updated price.");

    // Verify Physicals for Update
    // Laptop was in x_adds. Replacing it in x_adds.
    // Trigger logic: DELETE from x_adds WHERE id=OLD.id; INSERT INTO x_dels ...; INSERT INTO x_adds ...
    // So x_dels should contain the ID of the Laptop. (Because we hide the "old" version which was in adds? 
    // Actually, if it was in adds, it wasn't in base. But the trigger is generic. It hides whatever ID provided.)
    // So `dels_coll.find` will fail because it does `SELECT id, data ...`.
    // We must use raw SQL to check `products_dels`.
    
    let dels_res = db.query("SELECT id FROM products_dels", &[])?;
    println!("Content of x_dels: {:?}", dels_res);
    // id1 should be in dels (because update trigger adds it there)
    // id1 should be in dels. id1 is now string/UUID.
    assert!(dels_res.iter().any(|r| r["id"].as_str().unwrap() == id1));

    // 6. Delete Data
    println!("Deleting Mouse...");
    let query_del = std::collections::HashMap::from([("name".to_string(), json!("Mouse"))]);
    collection.delete_one(&query_del)?;

    // Verify View
    let items_final = collection.find(None)?;
    assert_eq!(items_final.len(), 1); // Only Laptop remains
    assert!(items_final.iter().any(|i| i["name"] == "Laptop"));
    
    // Verify x_dels
    let dels_res_2 = db.query("SELECT id FROM products_dels", &[])?;
    println!("Content of x_dels after delete: {:?}", dels_res_2);
    // id2 should be in dels
    // id2 should be in dels
    assert!(dels_res_2.iter().any(|r| r["id"].as_str().unwrap() == id2));

    println!("SUCCESS: Phase 2 Delta Layer logic verified!");

    // Clean up
    db.shutdown()?;
    
    Ok(())
}
