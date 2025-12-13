
from dbcenter import DBCenter, LocalStorageAdapter, SyncEngine

class TestMultiUserSync(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        print("\n" + "="*60)
        print("Starting DBCenter Multi-User Sync Test Suite")
        print("="*60)
        
        cls.storage_path = os.path.abspath(f"./storage_py_{int(time.time())}_{uuid.uuid4().hex[:8]}")
        os.makedirs(cls.storage_path, exist_ok=True)
        print(f"[Setup Sync] Storage path: {cls.storage_path}")

        # Unique paths for User A and User B
        uid = uuid.uuid4().hex[:8]
        cls.path_a = os.path.abspath(f"./db_a_{uid}")
        cls.path_b = os.path.abspath(f"./db_b_{uid}")

        # Cleanups
        for path in [cls.path_a, cls.path_b]:
            if os.path.exists(path):
                shutil.rmtree(path)

    @classmethod
    def tearDownClass(cls):
        print("\n[Teardown Sync] Cleaning up...")
        for path in [cls.path_a, cls.path_b]:
             try:
                 DBCenter.force_remove(path)
                 if os.path.exists(path): shutil.rmtree(path)
             except Exception as e:
                 print(f"Cleanup warning for {path}: {e}")
        
        if os.path.exists(cls.storage_path):
            try:
                shutil.rmtree(cls.storage_path)
            except Exception as e:
                print(f"Storage cleanup warning: {e}")

    def test_sync_flow(self):
        """Verify Offline Sync -> Cloud -> Online Sync -> Vacuum flow."""
        print("\n--- Sync Test: Full Lifecycle ---")

        # --- User A (Offline) ---
        print("\n[User A] Initializing Offline DB...")
        # Use context manager for auto-shutdown
        with DBCenter(path=self.path_a, persist=False, service=False, multi_user_sync=True) as db_a:
            col_a = db_a.collection("notes")
            
            print("[User A] Inserting 10 records...")
            for i in range(1, 11):
                col_a.insert_one({
                    "title": f"Note {i}", 
                    "content": "Offline content", 
                    "author": "User A"
                })
            
            print("[User A] Syncing to storage...")
            storage_a = LocalStorageAdapter(self.storage_path)
            sync_a = SyncEngine(db_a, storage_a)
            
            synced_count = sync_a.sync_table("notes", 100)
            print(f"[User A] Synced {synced_count} records.")
            self.assertEqual(synced_count, 10, "User A should have synced 10 records")

        print("[User A] Shutdown complete.")
        time.sleep(2) # Ensure file handles released

        # --- User B (Online) ---
        print("\n[User B] Initializing Online DB...")
        with DBCenter(path=self.path_b, persist=False, service=False, multi_user_sync=True) as db_b:
            col_b = db_b.collection("notes")
            storage_b = LocalStorageAdapter(self.storage_path)
            sync_b = SyncEngine(db_b, storage_b)
            
            print("[User B] Initial state (should be empty):")
            count_initial = col_b.count()
            print(f"Count: {count_initial}")
            self.assertEqual(count_initial, 0, "User B should start empty")
            
            print("[User B] Pulling from storage...")
            sync_b.pull_and_apply("notes")
            print("[User B] Apply Success.")
            
            print("[User B] Checking data...")
            count_after = col_b.count()
            print(f"User B Note Count: {count_after}")
            self.assertEqual(count_after, 10, "User B should have 10 notes after pull")
            
            # --- Vacuum ---
            print("\n[User B] Performing Vacuum...")
            files_before = storage_b.list("notes")
            print(f"Files before vacuum: {files_before}")
            # Expect _DELTA_ files
            self.assertTrue(any("_DELTA_" in f for f in files_before), "Should have DELTA files before vacuum")

            sync_b.vacuum("notes")
            print("Vacuum Success.")
            
            files_after = storage_b.list("notes")
            print(f"Files after vacuum: {files_after}")
            # Expect _BASE_ file and NO _DELTA_
            self.assertTrue(any("_BASE_" in f for f in files_after), "Should have BASE file after vacuum")
            self.assertFalse(any("_DELTA_" in f for f in files_after), "Should NOT have DELTA files after vacuum")
            
            # Verify Integrity
            count_final = col_b.count()
            print(f"User B Note Count Post-Vacuum: {count_final}")
            self.assertEqual(count_final, 10, "Data loss detected after vacuum!")

        print("\n[Sync Test] Complete Success.")
