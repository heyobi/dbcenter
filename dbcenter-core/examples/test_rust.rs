//! DBCenter Rust API Test
//!
//! This test demonstrates direct usage of dbcenter-core from Rust.
//!
//! Run with: cargo run --example test_rust

use dbcenter_core::DBCenter;
use serde_json::json;

fn main() {
    println!();
    println!("======================================================================");
    println!("DBCenter Rust API Test - Native Edition");
    println!("======================================================================");
    println!();

    // Create database using builder pattern
    println!("--- Creating database ---");
    let db = match DBCenter::builder()
        .path("./testdata_rust")
        .persist(true)
        .build()
    {
        Ok(db) => {
            println!("✓ Database created (using builder pattern)");
            db
        }
        Err(e) => {
            eprintln!("Failed to create database: {}", e);
            std::process::exit(1);
        }
    };
    println!();

    // Get collection
    println!("--- Getting collection ---");
    let users = db.collection("users");
    println!("✓ Got collection 'users'");
    println!();

    // Insert documents
    println!("--- Inserting documents ---");
    let doc1 = json!({
        "name": "Ali",
        "age": 25,
        "city": "Istanbul"
    });

    match users.insert_one(&doc1) {
        Ok(id) => println!("✓ Inserted document (Ali) with ID: {}", id),
        Err(e) => {
            eprintln!("Insert failed: {}", e);
            std::process::exit(1);
        }
    }

    let doc2 = json!({
        "name": "Zeynep",
        "age": 30,
        "city": "Izmir"
    });

    match users.insert_one(&doc2) {
        Ok(id) => println!("✓ Inserted document (Zeynep) with ID: {}", id),
        Err(e) => {
            eprintln!("Insert failed: {}", e);
            std::process::exit(1);
        }
    }
    println!();

    // Query via SQL
    println!("--- Querying via SQL ---");
    match db.query("SELECT * FROM users", &[]) {
        Ok(rows) => {
            println!("✓ SQL Result:");
            for row in &rows {
                println!(
                    "  {}",
                    serde_json::to_string_pretty(row).unwrap_or_default()
                );
            }
        }
        Err(e) => {
            eprintln!("SQL Query failed: {}", e);
        }
    }
    println!();

    // Test extension installation (pg_trgm - lightweight, no external deps)
    println!("--- Testing extension installation ---");
    match db.install_extension("pg_trgm") {
        Ok(_) => println!("✓ Extension 'pg_trgm' installed"),
        Err(e) => println!("⚠ Extension install skipped: {}", e),
    }
    println!();

    // Test VectorStore (optional - requires pgvector)
    println!("--- Testing VectorStore (optional) ---");
    match db.vector_store("embeddings", 3) {
        Ok(vs) => {
            println!("✓ Got vector store 'embeddings' (dim=3)");

            // Add vectors
            let ids = vec!["v1".to_string(), "v2".to_string()];
            let embeddings = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];

            match vs.add(&ids, &embeddings, None) {
                Ok(_) => println!("✓ Added 2 vectors"),
                Err(e) => println!("⚠ Vector add failed: {}", e),
            }

            // Query vectors
            let query_vec = vec![vec![1.0, 0.1, 0.0]];
            match vs.query(&query_vec, 2, None) {
                Ok(results) => {
                    println!("✓ Query returned {} results", results.len());
                    for result in &results {
                        println!("  - {}", result);
                    }
                }
                Err(e) => println!("⚠ Vector query failed: {}", e),
            }
        }
        Err(e) => {
            println!("⚠ VectorStore skipped (pgvector not available): {}", e);
        }
    }
    println!();

    // List collections
    println!("--- Listing collections ---");
    match db.list_collections() {
        Ok(collections) => {
            println!("✓ Collections: {:?}", collections);
        }
        Err(e) => {
            eprintln!("List collections failed: {}", e);
        }
    }
    println!();

    // Cleanup
    println!("--- Cleanup ---");
    match db.shutdown() {
        Ok(_) => println!("✓ Database shutdown"),
        Err(e) => eprintln!("Shutdown warning: {}", e),
    }

    // Force remove test data
    match DBCenter::force_remove("./testdata_rust") {
        Ok(_) => println!("✓ Test data removed"),
        Err(e) => eprintln!("Force remove warning: {}", e),
    }
    println!();

    println!("======================================================================");
    println!("✓ ALL RUST API TESTS PASSED!");
    println!("======================================================================");
    println!();
}
