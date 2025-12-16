
import os
import time
import json
import uuid
from dbcenter import DBCenter, GoogleDriveAdapter, SyncEngine, init_logging

# Enable logging
#os.environ["RUST_LOG"] = "debug"
init_logging()

CLIENT_SECRET_FILE = "client_secret.json"
CLIENT_SECRET_DATA = {
  "installed": {
    "client_id": "263274208002-l6j75tos8kp7t4l5c3atijbal74v02im.apps.googleusercontent.com",
    "project_id": "your-project-id",
    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
    "token_uri": "https://oauth2.googleapis.com/token",
    "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
    "client_secret": "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",
    "redirect_uris": [
      "http://localhost:8080"
    ]
  }
}

def setup_client_secret():
    if not os.path.exists(CLIENT_SECRET_FILE):
        with open(CLIENT_SECRET_FILE, "w") as f:
            json.dump(CLIENT_SECRET_DATA, f, indent=2)
    print(f"Client secret written to {CLIENT_SECRET_FILE}")

def main():
    setup_client_secret()
    
    print("=== Google Drive Smart Scan + Scale Zero Test ===")
    
    # 1. Setup DB
    db_path = "./data_drive_test"
    DBCenter.force_shutdown(db_path)
    DBCenter.force_remove(db_path)
    
    db = DBCenter(path=db_path, multi_user_sync=True)
    collection_name = "smart_drive_data"
    
    # 2. Setup Google Drive
    print("Initializing Google Drive Adapter (Auth may be required)...")
    # Using 'dbcenter_store_test' as root folder
    storage = GoogleDriveAdapter(CLIENT_SECRET_FILE, "dbcenter_store_test")
    
    sync = SyncEngine(db, storage)
    
    # 3. Scale Zero: Insert Data (simulate Edge Node A)
    print("\n--- Phase 1: Ingestin Data (Edge Node A) ---")
    users = db.collection(collection_name)
    
    target_ids = []
    
    # Insert 100 records
    print("Inserting 100 records...")
    for i in range(100):
        # We use string ID for control, or let it generate UUID? 
        # For SmartScan test, we need to know the ID.
        # But SmartScan relies on Predicate Pushdown which usually works on 'id' column.
        # Let's insert with explicit ID to make testing easier, OR capture ID.
        # dbcenter auto-generates UUID v7 if not provided.
        # Let's use auto-gen and capture one.
        doc = {"name": f"User {i}", "value": i * 10, "status": "active"}
        uid = users.insert_one(doc)
        if i == 50:
            target_ids.append(uid)
            print(f"Target ID for Smart Scan: {uid}")
            
    # Sync to Drive (Push)
    print("Syncing (Push) to Google Drive...")
    # First vacuum to ensure we have a base file? 
    # Or just standard sync (delta)?
    # For Smart Scan to be effective (Range Request), we usually need Parquet structure (vacuumed).
    # Delta files are simpler. Smart Reader works on Parquet. both delta and base are parquet.
    # But usually smart scan is for large base files.
    sync.sync_table(collection_name, 1000)
    
    # Run Vacuum to create a OPTIMIZED Base File with statistics
    print("Running Vacuum to create optimized Base file on Drive...")
    sync.vacuum(collection_name)
    
    # Verify files on Drive
    print("Verifying files on Google Drive...")
    files = storage.list(collection_name)
    print(f"Files in '{collection_name}': {files}")
    if not any(f.endswith(".parquet") for f in files):
        print("WARNING: No parquet files found in Drive!")
    
    # 4. Scale Zero: Reset Local State (Simulate Edge Node B / Cold Start)
    print("\n--- Phase 2: Cold Start (Edge Node B) ---")
    del sync
    del users
    del db
    DBCenter.force_shutdown(db_path)
    DBCenter.force_remove(db_path)
    
    # Re-open empty DB
    print("Opening fresh DB...")
    db_b = DBCenter(path=db_path, multi_user_sync=True)
    storage_b = GoogleDriveAdapter(CLIENT_SECRET_FILE, "dbcenter_store_test")
    sync_b = SyncEngine(db_b, storage_b)
    
    # 5. Smart Scan Request (Predicate Pushdown)
    print("\n--- Phase 3: Smart Scan ---")
    target_id = target_ids[0]
    print(f"Executing Smart Query for ID: {target_id}")
    print("This should use Range Requests to download only the relevant Row Group from Drive.")
    
    start_t = time.time()
    results = sync_b.smart_query(collection_name, target_id)
    duration = time.time() - start_t
    
    print(f"Smart Query completed in {duration:.2f}s")
    print(f"Found {len(results)} records")
    
    if len(results) > 0:
        print("SUCCESS: Record found via Smart Scan!")
        print(f"Sample: {results[0]}")
    else:
        print("FAILURE: Record NOT found.")
        exit(1)

    print("\nTest Completed Successfully.")

if __name__ == "__main__":
    main()
