from dbcenter import DBCenter
import os
import time

DB_PATH = "./test_hang_db"
# Should exist from previous run

print("Connecting...")
db = DBCenter(path=DB_PATH, persist=True)

print("Raw Update...")
try:
    sql = """
    UPDATE "users" 
    SET "name" = 'UpdatedCheckRaw', updated_at = CURRENT_TIMESTAMP
    WHERE id = (
        SELECT id FROM "users" 
        WHERE "name" = 'HangCheck' 
        LIMIT 1
    )
    """
    db.query(sql) # db.query supports SELECT usually. For UPDATE it might return empty list or fail if it expects rows. 
    # Wrapper query() maps results. execute() returns count.
    # Python wrapper only exposes query().
    # Core query() returns Rows. UPDATE returning nothing returns 0 rows.
    
    print("Raw Update Done.")
except Exception as e:
    print(f"Raw Update Error: {e}")

print("Done.")
db.shutdown()
