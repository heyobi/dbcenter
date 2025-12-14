
import time
import threading
import os
import shutil
import uuid
import json
from dbcenter import DBCenter, LocalStorageAdapter, SyncEngine

def setup():
    storage_path = os.path.abspath("./storage_concurrency")
    db_path = os.path.abspath("./db_concurrency")
    # Clean up previous runs
    if os.path.exists(storage_path): shutil.rmtree(storage_path)
    if os.path.exists(db_path): shutil.rmtree(db_path)
    os.makedirs(storage_path, exist_ok=True)
    return db_path, storage_path

def test_lock_contention():
    print("\n--- Test 1: Lock Contention ---")
    db_path, storage_path = setup()
    
    # Initialize storage and DB
    storage_user_a = LocalStorageAdapter(storage_path)
    
    lock_key = "test_table_vacuum.lock"
    worker_id_a = str(uuid.uuid4())
    
    print(f"User A acquiring lock manually ({worker_id_a})...")
    # Manually acquire the lock with 5s TTL
    acquired = storage_user_a.lock(lock_key, 5000, worker_id_a)
    if not acquired:
        print("FAIL: User A could not acquire lock initially.")
        return

    # Now verify lock file exists
    lock_file_path = os.path.join(storage_path, lock_key)
    if not os.path.exists(lock_file_path):
        print("FAIL: Lock file was not created on disk.")
        return
    print("User A holds the lock.")

    # Spawn thread for User B (Vacuum)
    def run_vacuum_b():
        print("User B starting vacuum... (should block/spin)")
        # Create separate db/storage instances for thread safety emulation
        _storage = LocalStorageAdapter(storage_path)
        with DBCenter(path=db_path, multi_user_sync=True) as _db:
             # Insert data to ensure vacuum works
            _db.collection("test_table").insert_one({"data": "foo"})
            _sync = SyncEngine(_db, _storage)
            
            start_t = time.time()
            try:
                _sync.vacuum("test_table")
                duration = time.time() - start_t
                print(f"User B vacuum COMPLETED in {duration:.2f}s")
            except Exception as e:
                print(f"User B vacuum FAILED: {e}")

    t = threading.Thread(target=run_vacuum_b)
    t.start()
    
    # Let B start and enter spin loop
    time.sleep(2) 
    print("User A holding lock for 2s...")
    
    # Unlock A
    print("User A releasing lock...")
    storage_user_a.unlock(lock_key, worker_id_a)
    
    # Wait for B
    t.join()
    print("Test 1 Finished.")

def test_stale_lock_recovery():
    print("\n--- Test 2: Stale Lock Recovery ---")
    db_path, storage_path = setup()
    
    storage = LocalStorageAdapter(storage_path)
    lock_key = "test_table_vacuum.lock"
    
    # Create EXPIRED lock manually
    stale_lock = {
        "expires_at": int((time.time() - 10) * 1000), # 10s ago
        "worker_id": "dead_worker"
    }
    with open(os.path.join(storage_path, lock_key), "w") as f:
        json.dump(stale_lock, f)
        
    print("Created stale lock (dead_worker).")
    
    with DBCenter(path=db_path, multi_user_sync=True) as db:
        db.collection("test_table").insert_one({"data": "bar"})
        sync = SyncEngine(db, storage)
        
        print("User B starting vacuum (should recover/overwrite lock immediately)...")
        start_t = time.time()
        sync.vacuum("test_table")
        duration = time.time() - start_t
        
        if duration < 5.0:
            print(f"SUCCESS: Vacuum finished quickly ({duration:.2f}s), indicating stale lock recovery.")
        else:
            print(f"WARNING: Vacuum took multiple seconds ({duration:.2f}s), might have span-waited unnecessarily?")

    # Verify lock file is gone (vacuum cleanup)
    if not os.path.exists(os.path.join(storage_path, lock_key)):
        print("SUCCESS: Lock file cleaned up after vacuum.")
    else:
        print("INFO: Lock file remains (maybe acceptable if release failed, but strictly should be gone).")

def test_atomic_swap():
    print("\n--- Test 3: Atomic Swap Verification ---")
    db_path, storage_path = setup()
    
    # We want to check that after vacuum, we have a BASE file and NO .tmp files
    with DBCenter(path=db_path, multi_user_sync=True) as db:
        coll = db.collection("test_table")
        for i in range(10):
            coll.insert_one({"i": i})
        
        sync = SyncEngine(db, LocalStorageAdapter(storage_path))
        sync.vacuum("test_table")
        
    files = os.listdir(storage_path)
    print(f"Files in storage after vacuum: {files}")
    
    base_files = [f for f in files if "_BASE_" in f and f.endswith(".parquet")]
    tmp_files = [f for f in files if f.endswith(".tmp")]
    
    if base_files and not tmp_files:
        print("SUCCESS: Atomic swap appeared to work (Base exists, no Tmp).")
    elif tmp_files:
        print(f"FAIL: Found .tmp files: {tmp_files}")
    else:
        print("FAIL: No BASE file found?")

if __name__ == "__main__":
    try:
        test_lock_contention()
        test_stale_lock_recovery()
        test_atomic_swap()
    except Exception as e:
        print(f"\nCRITICAL ERROR in Test Suite: {e}")
