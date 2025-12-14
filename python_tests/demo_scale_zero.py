import dbcenter
import os
import shutil
import time

# --- Configuration ---
STORAGE_DIR = os.path.abspath("./storage_demo_mb")
DB_INIT_path = os.path.abspath("./db_init")
DB_A_PATH = os.path.abspath("./db_node_a")
DB_B_PATH = os.path.abspath("./db_node_b")

def clean_paths():
    for p in [STORAGE_DIR, DB_INIT_path, DB_A_PATH, DB_B_PATH]:
        if os.path.exists(p):
            try:
                shutil.rmtree(p)
            except:
                pass 
    os.makedirs(STORAGE_DIR, exist_ok=True)

def print_header(msg):
    print(f"\n{'='*60}")
    print(f" {msg}")
    print(f"{'='*60}")

def main():
    clean_paths()

    # ---------------------------------------------------------
    # 1. SETUP: Create "Big Data" (500 records) in Shared Storage
    # ---------------------------------------------------------
    print_header("1. SETUP: Simulating 300GB Data (500 rows) on Shared Storage")
    
    # Initialize "Cloud" creator node
    stor_init = dbcenter.LocalStorageAdapter(STORAGE_DIR)
    # Enable Multi-User Sync Mode (Phase 2 requirement for Delta Layer)
    db_init = dbcenter.DBCenter(path=DB_INIT_path, multi_user_sync=True)
    sync_init = dbcenter.SyncEngine(db_init, stor_init)
    col_init = db_init.collection("largedata")

    print("- Inserting 500 records...")
    for i in range(500):
        col_init.insert_one({
            "id": i, 
            "content": f"Historical Data Record {i} - This represents a large historical dataset.",
            "type": "archive"
        })
    
    # Sync and Vacuum to create a single large BASE file
    print("- Syncing & Vacuuming to create BASE file...")
    sync_init.sync_table("largedata", 1000)
    sync_init.vacuum("largedata")
    
    # Verify file existence
    files = os.listdir(STORAGE_DIR)
    base_files = [f for f in files if "BASE" in f]
    print(f"- Storage State: {len(base_files)} BASE file(s) created.")
    
    # Cleanup init node to ensure we are testing "Fresh" nodes later
    del db_init
    del sync_init
    print("- Waiting for shutdown...")
    time.sleep(3) # Wait for Watcher to stop PostgreSQL
    
    # Retry loop for Windows file locks
    for i in range(5):
        try:
            if os.path.exists(DB_INIT_path):
                shutil.rmtree(DB_INIT_path)
            break
        except Exception as e:
            print(f"  Cleanup retry {i+1}: {e}")
            time.sleep(2)
            
    print("- Init node destroyed. Data exists ONLY in Shared Storage now.")


    # ---------------------------------------------------------
    # 2. NODE A: Lazy Insert (Partial Sync)
    # ---------------------------------------------------------
    print_header("2. NODE A: Lazy Insert (Scale-to-Zero)")
    
    stor_a = dbcenter.LocalStorageAdapter(STORAGE_DIR)
    db_a = dbcenter.DBCenter(path=DB_A_PATH, multi_user_sync=True)
    sync_a = dbcenter.SyncEngine(db_a, stor_a)
    col_a = db_a.collection("largedata")

    print("- Node A performing PARTIAL SYNC (partial=True)...")
    # This should download DELTAS (none exist) and SKIP the BASE (500 records)
    sync_a.pull_and_apply("largedata", partial=True)

    # Verify Local State
    count_a = db_a.query("SELECT COUNT(*) FROM largedata_base")[0]['count']
    print(f"- Node A Local Record Count: {count_a}")
    if count_a == 0:
        print("✅ SUCCESS: Node A did NOT download the 300GB history!")
    else:
        print(f"❌ FAIL: Node A downloaded {count_a} records.")

    print("- Node A inserting 1 new 'Edge' record...")
    col_a.insert_one({"msg": "New Edge Data from Node A", "type": "live"})
    
    print("- Node A syncing change to storage...")
    sync_a.sync_table("largedata", 10)
    
    # Cleanup Node A to release any locks/resources
    print("- Node A going offline...")
    del db_a
    del sync_a
    time.sleep(2)


    # ---------------------------------------------------------
    # 3. NODE B: Read New Data (Partial Sync)
    # ---------------------------------------------------------
    print_header("3. NODE B: Reading New Data (Partial Sync)")
    
    stor_b = dbcenter.LocalStorageAdapter(STORAGE_DIR)
    db_b = dbcenter.DBCenter(path=DB_B_PATH, multi_user_sync=True)
    sync_b = dbcenter.SyncEngine(db_b, stor_b)
    
    print("- Node B performing PARTIAL SYNC...")
    # This should download ONLY the 1 new Delta from Node A
    sync_b.pull_and_apply("largedata", partial=True)
    
    count_b = db_b.query("SELECT COUNT(*) FROM largedata_base")[0]['count']
    print(f"- Node B Local Record Count: {count_b}")
    
    new_data = db_b.query("SELECT * FROM largedata_base WHERE data->>'type' = 'live'")
    if len(new_data) == 1:
        print(f"✅ SUCCESS: Node B found the new record: {new_data[0]['data']}")
    else:
        print("❌ FAIL: Node B did not find the new record.")

    if count_b < 500:
        print("✅ SUCCESS: Node B still hasn't downloaded the massive history.")


    # ---------------------------------------------------------
    # 4. VACUUM: Consolidate (Full Merge)
    # ---------------------------------------------------------
    print_header("4. VACUUM: Merging Delta into Base")
    print("- Node B starting Vacuum...")
    print("- NOTE: Vacuum requires FULL consistency, so it WILL download the Base now.")
    
    #sync_b.vacuum("largedata")
    
    count_final = db_b.query("SELECT COUNT(*) FROM largedata_base")[0]['count']
    print(f"- Node B Local Count after Vacuum: {count_final}")
    
    if count_final == 501:
        print("✅ SUCCESS: All data (500 History + 1 New) verified and merged.")
    else:
        print(f"❌ FAIL: Count mismatch. Expected 501, got {count_final}")

    files_final = os.listdir(STORAGE_DIR)
    print(f"- Final Storage Files: {files_final}")
    if len([f for f in files_final if "BASE" in f]) == 1 and len([f for f in files_final if "DELTA" in f]) == 0:
         print("✅ SUCCESS: Storage compacted to single BASE file.")

if __name__ == "__main__":
    main()
