use dbcenter_core::DBCenter;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("Creating DBCenter...");
    let db = DBCenter::new(Some("./rust_test_db"), false, false, None, None, None, None)?;

    println!("Getting collection...");
    let users = db.collection("test_users");

    println!("Inserting document...");
    let doc = json!({"name": "Alice", "age": 25});
    println!("Document: {}", doc);

    match users.insert_one(&doc) {
        Ok(id) => println!("Success! ID: {}", id),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            eprintln!("Details: {}", e);
        }
    }

    println!("Shutting down...");
    db.shutdown()?;

    Ok(())
}
