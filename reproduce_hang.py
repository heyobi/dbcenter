from dbcenter import DBCenter
import os
import shutil
import time

DB_PATH = "./test_hang_db"

print(f"Cleaning up {DB_PATH}...")
try:
    DBCenter.force_shutdown(DB_PATH)
    time.sleep(2)
except:
    pass

if os.path.exists(DB_PATH):
    import shutil
    try:
        shutil.rmtree(DB_PATH, ignore_errors=True)
    except Exception as e:
        print(f"Warning: rmtree failed: {e}")

print("Init DB...")
db = DBCenter(path=DB_PATH, persist=True)
coll = db.collection("users")

print("Inserting...")
uid = coll.insert_one({"name": "HangCheck"})
print(f"Inserted: {uid}")

print("Finding...")
# This is where it allegedly hangs
doc = coll.find_one({"name": "HangCheck"})
print(f"Found: {doc}")

print("Updating...")
coll.update_one({"name": "HangCheck"}, {"$set": {"name": "UpdatedCheck"}})
print("Update done.")

print("Finding updated...")
doc2 = coll.find_one({"name": "UpdatedCheck"})
print(f"Found updated: {doc2}")

print("Done.")
db.shutdown()
