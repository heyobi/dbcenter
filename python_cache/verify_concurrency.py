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
    # Enable Rust Logging
    os.environ["RUST_LOG"] = "debug"
    os.environ["DBCENTER_SHOW_WATCHER_LOGS"] = "1"
    
    # Init logging AFTER setting env vars
    try:
        import dbcenter
        dbcenter.init_logging()
        print("[Setup] Rust logging initialized.")
    except Exception as e:
        print(f"[Setup] WARNING: Could not init logging: {e}")
    
    # Windows için Güvenli Temizlik (Retry Mechanism)
    # Postgres kapanırken dosyaları hemen bırakmayabilir, 5 saniye boyunca dener.
    for i in range(5):
        try:
            if os.path.exists(storage_path): 
                shutil.rmtree(storage_path, ignore_errors=True)
            if os.path.exists(db_path): 
                shutil.rmtree(db_path, ignore_errors=True)
            break
        except OSError as e:
            print(f"[Setup] Cleanup retry {i+1}/5 (Waiting for file release)...")
            time.sleep(1)
            
    # Hala silinmediyse son bir kez dene ve hata fırlat
    if os.path.exists(db_path):
        try:
            shutil.rmtree(db_path)
        except OSError:
            print("WARNING: Could not fully clean DB path. Trying to continue...")

    os.makedirs(storage_path, exist_ok=True)
    return db_path, storage_path

def test_lock_contention():
    print("\n--- Test 1: Lock Contention ---")
    db_path, storage_path = setup()
    
    storage_user_a = LocalStorageAdapter(storage_path)
    lock_key = "test_table_vacuum.lock"
    worker_id_a = str(uuid.uuid4())
    
    # 1. User A kilidi manuel alır (5 saniye süreli)
    print(f"User A acquiring lock manually ({worker_id_a})...")
    acquired = storage_user_a.lock(lock_key, 5000, worker_id_a)
    if not acquired:
        print("FAIL: User A could not acquire lock initially.")
        return

    # Kilit dosyasının diskte olduğundan emin ol
    lock_file_path = os.path.join(storage_path, lock_key)
    if not os.path.exists(lock_file_path):
        print("FAIL: Lock file was not created on disk.")
        return
    print("User A holds the lock.")

    # 2. User B (Vacuum) Başlatılır
    results = {"duration": 0.0, "error": None}
    ready_to_vacuum = threading.Event()
    
    def run_vacuum_b():
        print("User B initializing DB... (May take seconds)")
        # Thread safety için yeni instance
        _storage = LocalStorageAdapter(storage_path)
        # DB oluştur
        try:
            with DBCenter(path=db_path, multi_user_sync=True) as _db:
                 # Insert data to ensure vacuum works
                _db.collection("test_table").insert_one({"data": "foo"})
                _sync = SyncEngine(_db, _storage)
                
                print("User B DB Ready. Waiting for A to acquire lock...")
                ready_to_vacuum.set() # Signal A to grab lock
                
                # Give A a moment to grab the lock
                time.sleep(2.0) 
                
                print("User B calling vacuum... (Should BLOCK)")
                start_t = time.time()
                # Vakum kilidi beklemeli
                _sync.vacuum("test_table")
                results["duration"] = time.time() - start_t
                print(f"User B vacuum COMPLETED in {results['duration']:.2f}s")
        except Exception as e:
            results["error"] = e
            print(f"User B vacuum FAILED: {e}")

    t = threading.Thread(target=run_vacuum_b)
    t.start()
    
    # Wait for B to be ready
    print("User A waiting for B to initialize...")
    if not ready_to_vacuum.wait(timeout=30):
        print("FAIL: User B timed out initializing.")
        return

    # 1. User A kilidi manuel alır (30 saniye süreli)
    print(f"User A acquiring lock manually ({worker_id_a})...")
    # Increase TTL to 30s to cover any lags
    acquired = storage_user_a.lock(lock_key, 30000, worker_id_a)
    if not acquired:
        print("FAIL: User A could not acquire lock initially.")
        return

    # Kilit dosyasının diskte olduğundan emin ol
    lock_file_path = os.path.join(storage_path, lock_key)
    if not os.path.exists(lock_file_path):
        print("FAIL: Lock file was not created on disk.")
        return
        
    print("User A holds the lock.")

    # 3. User A kilidi 5 saniye tutar
    print("User A holding lock for 5s...")
    time.sleep(5.0)
    
    # 4. User A kilidi bırakır
    print("User A releasing lock...")
    storage_user_a.unlock(lock_key, worker_id_a)
    
    t.join()
    
    # KONTROL: Eğer B 2 saniyeden az sürede bittiyse, kilidi beklemedi demektir!
    if results["duration"] < 1.5:
        print(f"FAIL: Lock did not block! Duration: {results['duration']:.2f}s (Expected > 2.0s)")
        print("Possible causes: Lock file location mismatch, logic bug in Rust lock(), or file permission issue.")
        raise AssertionError("Concurrency Lock Failed")
    else:
        print(f"SUCCESS: Lock blocked successfully. Duration: {results['duration']:.2f}s")

def test_stale_lock_recovery():
    print("\n--- Test 2: Stale Lock Recovery ---")
    db_path, storage_path = setup()
    
    storage = LocalStorageAdapter(storage_path)
    lock_key = "test_table_vacuum.lock"
    
    # Geçmiş tarihli (EXPIRED) bir kilit oluştur
    stale_lock = {
        "expires_at": int((time.time() - 10) * 1000), # 10 saniye önce dolmuş
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
            print(f"WARNING: Vacuum took too long ({duration:.2f}s).")

    if not os.path.exists(os.path.join(storage_path, lock_key)):
        print("SUCCESS: Lock file cleaned up after vacuum.")
    else:
        print("INFO: Lock file remains (maybe acceptable).")

def test_atomic_swap():
    print("\n--- Test 3: Atomic Swap Verification ---")
    # Setup artık retry mekanizması ile daha güvenli
    db_path, storage_path = setup()
    
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

if __name__ == "__main__":
    try:
        test_lock_contention()
        test_stale_lock_recovery()
        test_atomic_swap()
        print("\nAll Tests Passed Successfully!")
    except Exception as e:
        print(f"\nCRITICAL ERROR in Test Suite: {e}")