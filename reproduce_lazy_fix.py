
import dbcenter

def test_lazy_filter():
    print("--- LazyFrame correct usage ---")
    db = dbcenter.DBCenter("./test_lazy_db")
    coll = db.collection("users")
    coll.delete_many(None)
    
    # Insert data
    coll.insert_many([
        {"name": "Alice", "age": 25},
        {"name": "Bob", "age": 30},
        {"name": "Charlie", "age": 35}
    ])
    
    print("Data inserted.")
    
    # 1. Correct: Chaining
    print("\n[Test 1] Chaining:")
    results = coll.lazy().filter("age > 28").collect()
    print("Filtered Results (age > 28):", results)
    
    # 2. Correct: Reassignment
    print("\n[Test 2] Reassignment:")
    lazy = coll.lazy()
    lazy = lazy.filter("age < 32") # Reassigning catches the new object
    results2 = lazy.collect() # consumes lazy
    print("Filtered Results (age < 32):", results2)
    
    # 3. Incorrect: Reuse (Will fail)
    print("\n[Test 3] Reuse (Expected Failure):")
    lazy_reuse = coll.lazy()
    lazy_reuse.filter("age > 0") # Returns new object, discarded. lazy_reuse is consumed.
    try:
        lazy_reuse.collect()
    except Exception as e:
        print(f"Caught expected error: {e}")

    # 4. SQL Syntax Check
    print("\n[Test 4] SQL Syntax:")
    # Since filter injects into WHERE, we use standard SQL
    results3 = coll.lazy().filter("name = 'Bob'").collect()
    print("Filtered Results (name = 'Bob'):", results3)
    
if __name__ == "__main__":
    test_lazy_filter()
