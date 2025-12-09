from dbcenter import DBCenter
import os
import shutil
import time

TEST_DIR = os.path.join(os.getcwd(), "test_fuzzy_db")

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

print(f"Initializing DBCenter at {TEST_DIR}...")
db = DBCenter(path=TEST_DIR, persist=True, service=False, postgresql=17)

products = db.collection("products")

print("Inserting products...")
products.insert_many([
    {"name": "iPhone 15 Pro", "price": 999},
    {"name": "Samsung Galaxy S24", "price": 899},
    {"name": "iPad Air", "price": 599}
])

print("Running fuzzy search for 'ipone'...")
results = products.fuzzy_search(
    query="ipone",  # Typo!
    field="name",
    threshold=0.3
)

print(f"Results: {results}")

# Verify
success = False
for r in results:
    if "iPhone 15 Pro" in r['name'] and r.get('similarity', 0) > 0.3:
        success = True
        print(f"Found iPhone with similarity score: {r.get('similarity')}")

if success:
    print("SUCCESS: Fuzzy search found target.")
else:
    print("FAILURE: Fuzzy search missed target.")

print("Testing direct similarity...")
score = products.similarity("iPhone", "ipone")
print(f"Similarity('iPhone', 'ipone') = {score}")

if score > 0.4:
    print("SUCCESS: Similarity score is reasonable.")
else:
    print("FAILURE: Similarity score too low.")

db.shutdown()
