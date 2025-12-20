import sys
import os
import time

try:
    import pyarrow as pa
    print("PyArrow found")
except ImportError:
    print("PyArrow not found! Please install it.")
    sys.exit(1)

try:
    import polars as pl
    print("Polars found")
except ImportError:
    print("Polars not found (optional)")
    pl = None

try:
    from dbcenter import DBCenter
except ImportError:
    # Try importing from local build if not installed
    print("dbcenter not found in path, trying to add local target...")
    sys.path.append(os.path.join(os.getcwd(), "dbcenter-python"))
    try:
        from dbcenter import DBCenter
    except ImportError:
        print("Failed to import dbcenter")
        sys.exit(1)

def test_lazy():
    db_path = "./lazy_test_db"
    if os.path.exists(db_path):
        import shutil
        shutil.rmtree(db_path, ignore_errors=True)

    db = DBCenter(path=db_path, persist=True)
    coll = db.collection("users")

    print("\n[1] Inserting data...")
    coll.insert_many([
        {"name": "Alice", "age": 30, "score": 90.5, "active": True},
        {"name": "Bob", "age": 25, "score": 88.0, "active": False},
        {"name": "Charlie", "age": 35, "score": 95.0, "active": True},
    ])
    
    # Wait for data to be committed? insert_many is synchronous? Yes.
    
    print("\n[2] Testing LazyFrame Filter & Collect...")
    lazy = coll.lazy()
    filtered = lazy.filter('age > 28') # using string for now
    results = filtered.collect()
    print(f"Filtered Results (age > 28): {results}")
    
    # Expect Alice (30) and Charlie (35)
    assert len(results) == 2
    assert any(r['name'] == 'Alice' for r in results)
    
    print("\n[3] Testing Arrow Export...")
    arrow_table = coll.to_arrow()
    print(f"Arrow Table: {arrow_table}")
    print(f"Schema: {arrow_table.schema}")
    assert arrow_table.num_rows == 3
    assert "score" in arrow_table.column_names
    
    print("\n[4] Testing Lazy Arrow Export...")
    lazy_arrow = filtered.to_arrow()
    print(f"Lazy Filtered Arrow: {lazy_arrow}")
    assert lazy_arrow.num_rows == 2

    if pl:
        print("\n[5] Testing Polars Export...")
        df = coll.to_polars()
        print("Polars DataFrame:")
        print(df)
        assert df.height == 3
        assert df.filter(pl.col("name") == "Alice").height == 1
        
        print("\n[6] Testing Lazy Polars Export...")
        df_lazy = filtered.to_polars()
        print("Lazy Polars DF:")
        print(df_lazy)
        assert df_lazy.height == 2

    print("\nSuccess! All LazyFrame tests passed.")
    # Clean shutdown
    db.shutdown()

if __name__ == "__main__":
    test_lazy()
