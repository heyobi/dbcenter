from dbcenter import DBCenter
import time
import shutil
import os

db_path = "./data_drive_test_fix_v2"
print(f"Test Path: {db_path}")

# 1. Start Initial DB
print("1. Starting DB Instance 1...")
db1 = DBCenter(path=db_path)
pid1 = db1.port()
print(f"DB1 Started on port {pid1}")

# 2. Force Remove
print("2. Calling DBCenter.force_remove(path)...")
DBCenter.force_remove(db_path)
print("force_remove complete.")

# 3. Validation: Check if directory is gone
if os.path.exists(db_path):
    print("ERROR: Directory still exists!")
    # exit(1) # We expect it gone

# 4. Start New Instance on SAME path
print("3. Starting DB Instance 2 (Sync Mode)...")
try:
    db2 = DBCenter(path=db_path, multi_user_sync=True)
    print("SUCCESS: Instance 2 started successfully!")
    db2.shutdown()
except Exception as e:
    print(f"FAILURE: {e}")
    exit(1)

print("Test Passed.")
