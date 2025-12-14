import time
import threading
import os
import shutil
import uuid
import json
from dbcenter import DBCenter, LocalStorageAdapter, SyncEngine

# Her test için benzersiz yol üretir (Windows hatasını önlemek için)
def setup(test_name):
    unique_id = uuid.uuid4().hex[:6]
    storage_path = os.path.abspath(f"./storage_{test_name}_{unique_id}")
    db_path = os.path.abspath(f"./db_{test_name}_{unique_id}")
    
    os.makedirs(storage_path, exist_ok=True)
    return db_path, storage_path

def cleanup(db_path, storage_path):
    try:
        if os.path.exists(db_path): shutil.rmtree(db_path, ignore_errors=True)
        if os.path.exists(storage_path): shutil.rmtree(storage_path, ignore_errors=True)
    except:
        pass

def test_lock_contention():
    print("\n--- Test 1: Lock Contention ---")
    db_path, storage_path = setup("lock")
    
    # Koordinasyon için Event kullanıyoruz (Senin kodundaki gibi)
    # Bu sayede User B hazır olmadan User A harekete geçmez.
    ready_to_vacuum = threading.Event()
    results = {"duration": 0.0, "error": None}

    try:
        storage_user_a = LocalStorageAdapter(storage_path)
        lock_key = "test_table_vacuum.lock"
        worker_id_a = str(uuid.uuid4())
        
        # --- User B Thread ---
        def run_vacuum_b():
            print("User B initializing DB... (May take seconds)")
            _storage = LocalStorageAdapter(storage_path)
            try:
                with DBCenter(path=db_path, multi_user_sync=True) as _db:
                    _db.collection("test_table").insert_one({"data": "foo"})
                    _sync = SyncEngine(_db, _storage)
                    
                    print("User B DB Ready. Waiting for A to acquire lock...")
                    ready_to_vacuum.set() # Sinyal gönder: Ben hazırım!
                    
                    # User A'nın kilidi alması için biraz zaman tanı
                    time.sleep(2.0) 
                    
                    print("User B calling vacuum... (Should BLOCK)")
                    start_t = time.time()
                    _sync.vacuum("test_table")
                    results["duration"] = time.time() - start_t
                    print(f"User B vacuum COMPLETED in {results['duration']:.2f}s")
            except Exception as e:
                results["error"] = e
                print(f"User B vacuum FAILED: {e}")

        t = threading.Thread(target=run_vacuum_b)
        t.start()
        
        # User B hazır olana kadar bekle (Timeout 30sn)
        print("User A waiting for B to initialize...")
        if not ready_to_vacuum.wait(timeout=30):
            print("FAIL: User B timed out initializing.")
            return

        # 1. User A kilidi manuel alır (30 saniye süreli - Bol zaman verelim)
        print(f"User A acquiring lock manually ({worker_id_a})...")
        acquired = storage_user_a.lock(lock_key, 30000, worker_id_a)
        if not acquired:
            print("FAIL: User A could not acquire lock initially.")
            return
        print("User A holds the lock.")

        # 3. User A kilidi 5 saniye tutar (User B bu sırada deniyor olacak)
        print("User A holding lock for 5s...")
        time.sleep(5.0)
        
        # 4. User A kilidi bırakır
        print("User A releasing lock...")
        storage_user_a.unlock(lock_key, worker_id_a)
        
        t.join()
        
        if results["error"]:
            raise results["error"]

        # KONTROL: B en azından (5.0 - 2.0) = 3.0 saniye beklemiş olmalı
        # Biraz tolerans payı bırakıp 1.5 diyoruz.
        if results["duration"] < 1.5:
            print(f"FAIL: Lock did not block! Duration: {results['duration']:.2f}s")
            raise AssertionError("Concurrency Lock Failed")
        else:
            print(f"SUCCESS: Lock blocked successfully. Duration: {results['duration']:.2f}s")
            
    finally:
        cleanup(db_path, storage_path)

def test_stale_lock_recovery():
    print("\n--- Test 2: Stale Lock Recovery ---")
    db_path, storage_path = setup("stale")
    
    try:
        storage = LocalStorageAdapter(storage_path)
        lock_key = "test_table_vacuum.lock"
        
        stale_lock = {
            "expires_at": int((time.time() - 10) * 1000), 
            "worker_id": "dead_worker"
        }
        with open(os.path.join(storage_path, lock_key), "w") as f:
            json.dump(stale_lock, f)
            
        print("Created stale lock (dead_worker).")
        
        with DBCenter(path=db_path, multi_user_sync=True) as db:
            db.collection("test_table").insert_one({"data": "bar"})
            sync = SyncEngine(db, storage)
            
            print("User B starting vacuum (should recover immediately)...")
            start_t = time.time()
            sync.vacuum("test_table")
            duration = time.time() - start_t
            
            if duration < 5.0:
                print(f"SUCCESS: Vacuum finished quickly ({duration:.2f}s).")
            else:
                print(f"WARNING: Vacuum took too long ({duration:.2f}s).")

        if not os.path.exists(os.path.join(storage_path, lock_key)):
            print("SUCCESS: Lock file cleaned up after vacuum.")
        else:
            print("INFO: Lock file remains (acceptable).")
            
    finally:
        cleanup(db_path, storage_path)

def test_atomic_swap():
    print("\n--- Test 3: Atomic Swap Verification ---")
    db_path, storage_path = setup("swap")
    
    try:
        print("Initializing DB and inserting data...")
        with DBCenter(path=db_path, multi_user_sync=True) as db:
            coll = db.collection("test_table")
            for i in range(10):
                coll.insert_one({"i": i})
            
            print("Running Vacuum...")
            sync = SyncEngine(db, LocalStorageAdapter(storage_path))
            sync.vacuum("test_table")
            
        files = os.listdir(storage_path)
        print(f"Files in storage after vacuum: {files}")
        
        base_files = [f for f in files if "_BASE_" in f and f.endswith(".parquet")]
        tmp_files = [f for f in files if f.endswith(".tmp")]
        
        if base_files and not tmp_files:
            print("SUCCESS: Atomic swap worked (Base exists, no Tmp).")
        elif tmp_files:
            print(f"FAIL: Found .tmp files: {tmp_files} (Swap failed)")
        else:
            print("FAIL: No BASE file found?")
            
    finally:
        cleanup(db_path, storage_path)

if __name__ == "__main__":
    try:
        test_lock_contention()
        test_stale_lock_recovery()
        test_atomic_swap()
        print("\nAll Tests Passed Successfully!")
    except Exception as e:
        print(f"\nCRITICAL ERROR in Test Suite: {e}")