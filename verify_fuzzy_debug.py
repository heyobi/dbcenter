from dbcenter import DBCenter
import os
import shutil
import time

TEST_DIR = os.path.join(os.getcwd(), "test_fuzzy_db_debug")

# Clean up
if os.path.exists(TEST_DIR):
    try:
        DBCenter.force_shutdown(TEST_DIR)
        time.sleep(1)
    except:
        pass
    try:
        shutil.rmtree(TEST_DIR)
    except:
        pass

db = DBCenter(path=TEST_DIR, persist=True, service=False, postgresql=17)
products = db.collection("products")

products.insert_many([
    {"name": "iPhone 15 Pro", "price": 999},
])

# Debug similarity
sim = products.similarity("iPhone 15 Pro", "ipone")
print(f"Similarity('iPhone 15 Pro', 'ipone') = {sim}")

# Try case insensitive manually
sim_lower = products.similarity("iphone 15 pro", "ipone")
print(f"Similarity('iphone 15 pro', 'ipone') = {sim_lower}")

db.shutdown()
