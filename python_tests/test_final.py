import unittest
import os
import shutil
import time
import unittest
import os
import shutil
import time
import sys

from dbcenter import DBCenter, LocalStorageAdapter, SyncEngine

# Configuration
# Configuration
# Use a unique path to avoid Windows file locking issues from previous aborted runs
import uuid
TEST_DB_PATH = os.path.abspath(f"./test_db_{int(time.time())}_{uuid.uuid4().hex[:8]}")

class TestDBCenter(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        """Initialize the database environment once for all tests."""
        print("\n" + "="*60)
        print("Starting Professional DBCenter Test Suite")
        print("="*60)
        
        # 1. Clean up any leftovers from previous runs
        if os.path.exists(TEST_DB_PATH):
            print(f"[Setup] Cleaning up existing directory: {TEST_DB_PATH}")
            try:
                DBCenter.force_shutdown(TEST_DB_PATH)
                time.sleep(2) # Give it moments to release locks
                
                # Retry loop for deletion
                for i in range(5):
                    try:
                        if os.path.exists(TEST_DB_PATH):
                            shutil.rmtree(TEST_DB_PATH)
                        break
                    except Exception as e:
                        print(f"Cleanup attempt {i+1} failed: {e}")
                        time.sleep(1)
            except Exception as e:
                print(f"[Setup] Warning during cleanup: {e}")

        # 2. Initialize DBCenter
        print(f"[Setup] Initializing DBCenter at {TEST_DB_PATH}...")
        try:
            cls.db = DBCenter(path=TEST_DB_PATH, persist=True)
            print("[Setup] Database initialized successfully.")
        except Exception as e:
            print(f"[Setup] FAILED to initialize database: {e}")
            raise

    @classmethod
    def tearDownClass(cls):
        """Clean up the database environment after all tests."""
        print("\n" + "="*60)
        print("Teardown DBCenter Test Suite")
        print("="*60)
        
        if hasattr(cls, 'db') and cls.db:
            print("[Teardown] Shutting down database...")
            cls.db.shutdown()
        
        # Force remove just to be sure
        print(f"[Teardown] Force removing directory: {TEST_DB_PATH}")
        try:
             DBCenter.force_remove(TEST_DB_PATH)
             if os.path.exists(TEST_DB_PATH):
                shutil.rmtree(TEST_DB_PATH)
        except Exception as e:
            print(f"[Teardown] Warning during cleanup: {e}")

    def test_01_basic_collection_crud(self):
        """Test Basic CRUD operations on a standard collection."""
        print("\n--- Test 01: Basic Collection CRUD ---")
        users = self.db.collection("users")
        
        # create
        user_data = {"name": "Test User", "email": "test@example.com", "age": 30}
        user_id = users.insert_one(user_data)
        self.assertIsNotNone(user_id, "Insert failed, ID is None")
        print(f"Inserted user with ID: {user_id}")

        # read
        found_user = users.find_one({"name": "Test User"})
        self.assertIsNotNone(found_user, "Find failed, user not found")
        self.assertEqual(found_user['age'], 30)
        
        # update
        users.update_one({"_id": user_id}, {"$set": {"age": 31}})
        updated_user = users.find_one({"_id": user_id})
        self.assertEqual(updated_user['age'], 31, "Update failed")
        print("User updated successfully.")

        # delete
        users.delete_one({"_id": user_id})
        deleted_user = users.find_one({"_id": user_id})
        self.assertIsNone(deleted_user, "Delete failed, user still exists")
        print("User deleted successfully.")

    def test_02_vector_store(self):
        """Test Vector Store capabilities (pgvector)."""
        print("\n--- Test 02: Vector Store ---")
        # Initialize vector store
        dim = 3
        vstore = self.db.vector_store("test_vectors", dimension=dim)
        
        # Add vectors
        ids = ["vec1", "vec2", "vec3"]
        embeddings = [
            [0.1, 0.2, 0.3],
            [0.9, 0.8, 0.7],
            [0.11, 0.21, 0.31] # Similar to vec1
        ]
        metadatas = [{"tag": "A"}, {"tag": "B"}, {"tag": "A"}]
        
        vstore.add(ids=ids, embeddings=embeddings, metadatas=metadatas)
        print("Vectors added.")

        # Query
        query = [[0.1, 0.2, 0.3]]
        results = vstore.query(query_embeddings=query, n_results=2)
        
        self.assertEqual(len(results), 2, "Query did not return requested number of results")
        self.assertEqual(results[0]['id'], "vec1", "First result should be exact match vec1")
        print(f"Vector Query Results: {results}")

        # Metadata filtering
        results_filtered = vstore.query(query_embeddings=query, n_results=5, where={"tag": "B"})
        self.assertEqual(len(results_filtered), 1, "Filtered query count mismatch")
        self.assertEqual(results_filtered[0]['id'], "vec2", "Filtering returned wrong ID")
        print("Metadata filtering verified.")

    def test_03_geo_collection(self):
        """Test GeoSpatial capabilities (PostGIS)."""
        print("\n--- Test 03: Geo Collection ---")
        places = self.db.geo_collection("places")
        
        # Insert
        places.insert_many([
            {"name": "Center", "lat": 0.0, "lon": 0.0},
            {"name": "Point A", "lat": 0.01, "lon": 0.01}, # Nearby
            {"name": "Point B", "lat": 10.0, "lon": 10.0}   # Far away
        ])
        print("Map points inserted.")

        # Find within radius (approx 1 degree lat is ~111km, so 0.01 is ~1.1km)
        # Radius 5km should include Center and Point A
        nearby = places.find_within_radius(lat=0.0, lon=0.0, radius_km=5)
        
        names = [p['name'] for p in nearby]
        self.assertIn("Center", names)
        self.assertIn("Point A", names)
        self.assertNotIn("Point B", names)
        print(f"Radius search found: {names}")

    def test_04_extensions(self):
        """Test Extension Management."""
        print("\n--- Test 04: Extensions ---")
        # Try installing a standard extension
        try:
            self.db.install_extension("pg_trgm")
            print("Extension 'pg_trgm' installed successfully.")
            
            # Verify usage
            self.db.query("SELECT 'hello' % 'hell' as similar;")
            print("Extension usage verified.")
        except Exception as e:
            self.fail(f"Failed to install/use extension: {e}")


class TestDBCenterDelta(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        """Initialize a separate DBCenter instance with Delta Layer enabled."""
        print("\n" + "="*60)
        print("Starting DBCenter Delta Layer Test Suite")
        print("="*60)
        
        # Use a different path for Delta tests
        cls.delta_db_path = os.path.abspath(f"./test_db_delta_{int(time.time())}_{uuid.uuid4().hex[:8]}")
        
        print(f"[Setup Delta] Initializing with multi_user_sync=True at {cls.delta_db_path}...")
        try:
            # Enable multi_user_sync_mode=True
            cls.db = DBCenter(path=cls.delta_db_path, persist=True, multi_user_sync=True)
            print("[Setup Delta] Database initialized successfully.")
        except Exception as e:
            print(f"[Setup Delta] FAILED to initialize database: {e}")
            raise

    @classmethod
    def tearDownClass(cls):
        print("\n" + "="*60)
        print("Teardown DBCenter Delta Test Suite")
        print("="*60)
        
        if hasattr(cls, 'db') and cls.db:
            print("[Teardown Delta] Shutting down database...")
            cls.db.shutdown()
        
        print(f"[Teardown Delta] Force removing directory: {cls.delta_db_path}")
        try:
             DBCenter.force_remove(cls.delta_db_path)
             if os.path.exists(cls.delta_db_path):
                 shutil.rmtree(cls.delta_db_path)
        except Exception as e:
            print(f"[Teardown Delta] Warning during cleanup: {e}")

    def test_01_delta_architecture(self):
        """Verify Delta Layer architecture (physical tables + view)."""
        print("\n--- Delta Test 01: Architecture Verification ---")
        
        # 1. Create a collection
        coll_name = "products"
        products = self.db.collection(coll_name)
        
        # Insert a doc to ensure table creation triggers
        products.insert_one({"name": "TriggerInit", "price": 0})
        
        # 2. Check for physical tables using SQL
        # We expect 'products_base', 'products_adds', 'products_dels' in pg_tables
        tables = self.db.query("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
        table_names = [t['tablename'] for t in tables]
        print(f"Found tables: {table_names}")
        
        self.assertIn(f"{coll_name}_base", table_names, "Base table missing")
        self.assertIn(f"{coll_name}_adds", table_names, "Adds table missing")
        self.assertIn(f"{coll_name}_dels", table_names, "Dels table missing")
        
        # The main name 'products' should NOT be in pg_tables (it's a view), unless pg_tables includes views?
        # pg_tables does NOT include views. pg_views does.
        # Let's verify it is a view.
        views = self.db.query("SELECT viewname FROM pg_views WHERE schemaname = 'public'")
        view_names = [v['viewname'] for v in views]
        print(f"Found views: {view_names}")
        self.assertIn(coll_name, view_names, "Main view missing")

    def test_02_delta_crud_logic(self):
        """Verify Insert/Update/Delete logic routed to delta tables."""
        print("\n--- Delta Test 02: CRUD Logic ---")
        products = self.db.collection("products")
        
        # INSERT
        doc_id = products.insert_one({"name": "Laptop", "price": 1000})
        print(f"Inserted Laptop with ID: {doc_id}")
        
        # Verify it's in _adds
        adds_content = self.db.query("SELECT * FROM products_adds")
        self.assertTrue(any(row['data']['name'] == 'Laptop' for row in adds_content if 'data' in row), "Laptop not found in _adds")
        
        # UPDATE
        products.update_one({"_id": doc_id}, {"$set": {"price": 1200}})
        print("Updated Laptop price.")
        
        # Verify View (should see update)
        doc = products.find_one({"_id": doc_id})
        self.assertEqual(doc['price'], 1200, "View does not show updated price")
        
        # Verify _dels (Optimization check: update trigger likely puts OLD id in _dels to hide old version)
        # However, since the row was only in _adds, the trigger might just update _adds directly OR 
        # (based on our impl) it deletes from _adds and re-inserts? 
        # Implementation Detail: 
        # IF EXISTS in ADDS -> DELETE from ADDS. 
        # INSERT into DELS (to hide BASE if exists).
        # INSERT into ADDS (new version).
        # So yes, _dels might have an entry, and _adds has the new version.
        dels_content = self.db.query("SELECT * FROM products_dels")
        print(f"Dels content: {dels_content}")
        # Note: Depending on logic, if it wasn't in base, adding to dels is harmless but redundant.
        
        # DELETE
        products.delete_one({"_id": doc_id})
        print("Deleted Laptop.")
        
        # Verify View (should be gone)
        doc = products.find_one({"_id": doc_id})
        self.assertIsNone(doc, "Laptop still visible in view after delete")
        
        # Verify _dels (should definitely be here)
        dels_content_final = self.db.query("SELECT * FROM products_dels")
        # We check if ID is present. Note: query returns JSON.
        id_found = False
        for row in dels_content_final:
             if row.get('id') == doc_id:
                 id_found = True
                 break
        self.assertTrue(id_found, "Deleted ID NOT found in _dels table")


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
             print(f"[Teardown Sync] Removing {path}...")
             try:
                 print(f"[Teardown Sync] Calling DBCenter.force_remove({path})...")
                 DBCenter.force_remove(path)
                 print(f"[Teardown Sync] force_remove returned for {path}.")
                 if os.path.exists(path): 
                     print(f"[Teardown Sync] shutil.rmtree({path})...")
                     shutil.rmtree(path)
                     print(f"[Teardown Sync] shutil.rmtree finished for {path}.")
             except Exception as e:
                 print(f"[Teardown Sync] Cleanup warning for {path}: {e}")
        
        if os.path.exists(cls.storage_path):
            try:
                print(f"[Teardown Sync] Removing storage {cls.storage_path}...")
                shutil.rmtree(cls.storage_path)
                print("[Teardown Sync] Storage removed.")
            except Exception as e:
                print(f"[Teardown Sync] Storage cleanup warning: {e}")

    def test_sync_flow(self):
        """Verify Offline Sync -> Cloud -> Online Sync -> Vacuum flow."""
        print("\n--- Sync Test: Full Lifecycle ---")

        # --- User A (Offline) ---
        print("\n[User A] Initializing Offline DB...")
        # Use context manager for auto-shutdown
        with DBCenter(path=self.path_a, persist=True, service=False, multi_user_sync=True) as db_a:
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
        with DBCenter(path=self.path_b, persist=True, service=False, multi_user_sync=True) as db_b:
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
        
        # Explicit cleanup to allow background threads to exit
        if 'sync_b' in locals(): del sync_b
        if 'sync_a' in locals(): del sync_a
        print("[Sync Test] Waiting 2s for background threads to exit...")
        time.sleep(2)


if __name__ == '__main__':
    unittest.main()
