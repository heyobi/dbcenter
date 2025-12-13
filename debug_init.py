import os
import sys
import time
from dbcenter import DBCenter
import logging

# Configure logging
logging.basicConfig(level=logging.DEBUG)

DB_PATH = os.path.abspath("./debug_db")

print(f"Debug Script Started. DB Path: {DB_PATH}")
print(f"PID: {os.getpid()}")

# Force cleanup manually first
if os.path.exists(DB_PATH):
    print("Removing existing DB path with force_remove...")
    try:
        DBCenter.force_remove(DB_PATH)
    except Exception as e:
        print(f"force_remove error: {e}")
    
    time.sleep(1)
    if os.path.exists(DB_PATH):
        try:
            import shutil
            shutil.rmtree(DB_PATH)
            print("Removed via shutil")
        except Exception as e:
            print(f"shutil error: {e}")

print("Initializing DBCenter...")
try:
    # Enable Rust logging via env var for this process
    os.environ["RUST_LOG"] = "debug"
    
    start_time = time.time()
    db = DBCenter(path=DB_PATH, persist=True)
    end_time = time.time()
    
    print(f"DBCenter Initialized successfully in {end_time - start_time:.2f}s")
    print(f"Instance ID: {db.instance_id}")
    print(f"Port: {db.port}")
    
    print("Shutting down...")
    db.shutdown()
    print("Shutdown complete.")
    
except Exception as e:
    print(f"FATAL ERROR: {e}")
    import traceback
    traceback.print_exc()

print("Debug Script Finished.")
