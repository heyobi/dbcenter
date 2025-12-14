
import os
import shutil
import time
import dbcenter

# Configuration
TEST_DIR = os.path.abspath("test_smart_env")
STORAGE_DIR = os.path.join(TEST_DIR, "storage")
DB_INIT_PATH = os.path.join(TEST_DIR, "db_init")
DB_READER_PATH = os.path.join(TEST_DIR, "db_reader")

def setup_env():
    if os.path.exists(TEST_DIR):
        shutil.rmtree(TEST_DIR, ignore_errors=True)
        time.sleep(1)
    os.makedirs(STORAGE_DIR, exist_ok=True)

def print_header(msg):
    print(f"\n{'='*60}\n {msg}\n{'='*60}")

def main():
    setup_env()
    print_header("1. SETUP: Create Remote Parquet File (Sorted)")

    # Init User: Creates 1000 sorted records
    os.makedirs(DB_INIT_PATH)
    stor_init = dbcenter.LocalStorageAdapter(STORAGE_DIR)
    # Enable multi_user_sync for delta logic, though we just vacuum base here
    db_init = dbcenter.DBCenter(path=DB_INIT_PATH, multi_user_sync=True)
    sync_init = dbcenter.SyncEngine(db_init, stor_init)
    col_init = db_init.collection("notes")

    print("- Inserting 1000 records...")
    for i in range(1000):
        # ID is padded string "00000", "00001", ... "00999" for easy sorting
        col_init.insert_one({
            "id": f"{i:05}", 
            "content": f"Note content for {i}", 
            "tags": ["test", "smart"]
        })
    
    print("- Vacuuming to create sorted BASE file...")
    sync_init.sync_table("notes", 2000)
    sync_init.vacuum("notes")
    
    # Cleanup Init
    del db_init
    del sync_init
    time.sleep(2)
    shutil.rmtree(DB_INIT_PATH, ignore_errors=True)


    print_header("2. READER: Smart Scan (Predicate Pushdown)")
    
    # Reader has NO local data
    os.makedirs(DB_READER_PATH)
    stor_reader = dbcenter.LocalStorageAdapter(STORAGE_DIR)
    db_reader = dbcenter.DBCenter(path=DB_READER_PATH, multi_user_sync=True)
    sync_reader = dbcenter.SyncEngine(db_reader, stor_reader)
    
    # Verify .meta cache does not exist yet
    meta_dir = os.path.join(DB_READER_PATH, ".meta")
    if os.path.exists(meta_dir):
        print(f"WARN: .meta dir exists prematurely: {meta_dir}")

    target_id = "00500"
    print(f"- Executing smart_query for id='{target_id}'...")
    start_time = time.time()
    results = sync_reader.smart_query("notes", target_id)
    duration = time.time() - start_time
    
    print(f"- Query returned {len(results)} rows in {duration:.4f}s")
    print(f"- Result: {results}")

    if len(results) == 1:
        row = results[0]
        # Check data structure logic from SyncEngine
        data = row.get("data", {})
        if data.get("id") == target_id:
             print("✅ SUCCESS: Found correct record!")
        else:
             print(f"❌ FAIL: ID mismatch. Got {data.get('id')}")
    else:
        print(f"❌ FAIL: Expected 1 record, got {len(results)}")

    print_header("3. VERIFY: Metadata Cache")
    
    # Check if .meta file was created
    if os.path.exists(meta_dir):
        files = os.listdir(meta_dir)
        print(f"- Found .meta files: {files}")
        if len(files) > 0 and files[0].endswith(".parquet.meta"):
            print("✅ SUCCESS: Metadata cache file created.")
        else:
            print("❌ FAIL: No .meta file found.")
    else:
        print("❌ FAIL: .meta directory not found.")
        
    print_header("4. VERIFY: Cache Hit (2nd Query)")
    
    # Run again, should be faster/not download footer
    start_time = time.time()
    results2 = sync_reader.smart_query("notes", "00999")
    duration2 = time.time() - start_time
    print(f"- 2nd Query (cached meta) took {duration2:.4f}s")
    
    if len(results2) == 1 and results2[0].get("data", {}).get("id") == "00999":
         print("✅ SUCCESS: 2nd query correct.")
    else:
         print("❌ FAIL: 2nd query failed.")

    # Cleanup
    del db_reader
    del sync_reader
    # shutil.rmtree(TEST_DIR, ignore_errors=True) # Keep for inspection on fail
    print("\nDone.")

if __name__ == "__main__":
    main()
