
import time
import os
import shutil
import uuid
from dbcenter import DBCenter, LocalStorageAdapter, SyncEngine

def run_test():
    storage_path = os.path.abspath("./storage_lww")
    path_a = os.path.abspath("./db_lww_a")
    path_b = os.path.abspath("./db_lww_b")

    # Cleanup
    for p in [storage_path, path_a, path_b]:
        if os.path.exists(p):
            shutil.rmtree(p)
    os.makedirs(storage_path, exist_ok=True)

    print("\n--- User A: Create & Sync ---")
    with DBCenter(path=path_a, multi_user_sync=True) as db:
        col = db.collection("notes")
        id_a = col.insert_one({"title": "Original"})
        print(f"User A inserted {id_a}")
        
        sync = SyncEngine(db, LocalStorageAdapter(storage_path))
        sync.sync_table("notes", 100)

    time.sleep(1)

    print("\n--- User B: Pull ---")
    id_b = None
    with DBCenter(path=path_b, multi_user_sync=True) as db:
        col = db.collection("notes")
        sync = SyncEngine(db, LocalStorageAdapter(storage_path))
        sync.pull_and_apply("notes")
        
        docs = col.find(None)
        print(f"User B docs: {len(docs)}")
        if len(docs) > 0:
            doc_b = docs[0]
            # Handle different id keys just in case
            id_b = doc_b.get("_id") or doc_b.get("id")
            print(f"User B doc ID: {id_b}")

    if not id_b:
        print("Failed to sync doc to B")
        return

    time.sleep(1)

    print("\n--- User B: Update Locally ---")
    with DBCenter(path=path_b, multi_user_sync=True) as db:
        col = db.collection("notes")
        # Update
        col.update_one({"_id": id_b}, {"$set": {"title": "User B Local Update"}})
        
        d = col.find_one({"_id": id_b})
        print(f"User B Local Title: {d.get('title')}")

    time.sleep(1)

    print("\n--- User A: Update & Sync (Later TS) ---")
    # Used strictly later timestamp by waiting
    with DBCenter(path=path_a, multi_user_sync=True) as db:
        col = db.collection("notes")
        # Assuming ID A matches ID B (UUID string)
        col.update_one({"_id": id_b}, {"$set": {"title": "User A Cloud Update"}})
        
        sync = SyncEngine(db, LocalStorageAdapter(storage_path))
        sync.sync_table("notes", 100)

    time.sleep(1)

    print("\n--- User B: Pull Conflict (Expect No Crash) ---")
    with DBCenter(path=path_b, multi_user_sync=True) as db:
        sync = SyncEngine(db, LocalStorageAdapter(storage_path))
        try:
            sync.pull_and_apply("notes")
            print("Pull successful!")
        except Exception as e:
            print(f"CRASH DETECTED: {e}")
            exit(1)
            
        col = db.collection("notes")
        d = col.find_one({"_id": id_b})
        print(f"Final User B Title: {d.get('title')}")
        
    print("\nTest Complete")

if __name__ == "__main__":
    run_test()
