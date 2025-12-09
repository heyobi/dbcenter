import unittest
import os
import shutil
import time
import unittest
import os
import shutil
import time
import sys

from dbcenter import DBCenter

# Configuration
TEST_DB_PATH = os.path.abspath("./test_db_final")

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
                if os.path.exists(TEST_DB_PATH):
                    shutil.rmtree(TEST_DB_PATH)
            except Exception as e:
                print(f"[Setup] Warning during cleanup: {e}")

        # 2. Initialize DBCenter
        print(f"[Setup] Initializing DBCenter at {TEST_DB_PATH}...")
        cls.db = DBCenter(path=TEST_DB_PATH, persist=True)
        print("[Setup] Database initialized successfully.")

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

if __name__ == '__main__':
    unittest.main()
