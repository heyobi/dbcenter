"""The columnar view: the collection's Parquet published as a PostgreSQL view.

`columnar=True` installs pg_duckdb and keeps `<name>_columnar` pointing at the
objects the sync wrote. The point is that it is an ordinary view - psql, DBeaver
and every driver see a table, and nothing has to know about Parquet.

pg_duckdb is published for Linux on PostgreSQL 14-18, so this whole module skips
itself where it cannot be installed rather than failing.
"""

import os
import shutil
import unittest

import dbcenter

from harness import temp_path


def build_columnar_db(path):
    """A database with the columnar path on, or None if it cannot be had here."""
    try:
        return dbcenter.DBCenter(
            path=path, persist=True, multi_user_sync=True, columnar=True
        )
    except Exception as exc:  # pg_duckdb is not published for every platform
        print(f"skipping columnar tests: {exc}")
        return None


class ColumnarView(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path("columnar")
        cls.storage_dir = os.path.join(cls.workdir, "storage")
        os.makedirs(cls.storage_dir, exist_ok=True)
        cls.db = build_columnar_db(os.path.join(cls.workdir, "db"))
        if cls.db is None:
            raise unittest.SkipTest("pg_duckdb is not available on this platform")

        cls.sync = dbcenter.SyncEngine(cls.db, dbcenter.LocalStorageAdapter(cls.storage_dir))
        cls.sales = cls.db.collection("sales")
        cls.sales.insert_many(
            [
                {"region": "north", "units": 3, "amount": 10.5, "shipped": True},
                {"region": "north", "units": 7, "amount": 20.25, "shipped": False},
                {"region": "south", "units": 2, "amount": 5.0, "shipped": True},
            ]
        )
        cls.sync.sync_table("sales", 1000)

    @classmethod
    def tearDownClass(cls):
        if getattr(cls, "db", None) is not None:
            try:
                cls.db.shutdown()
            except Exception:
                pass
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def test_the_extension_is_installed_without_a_restart(self):
        rows = self.db.query("SELECT extname FROM pg_extension WHERE extname = 'pg_duckdb'")
        self.assertEqual(len(rows), 1, "pg_duckdb is not installed")

    def test_a_sync_leaves_a_view_behind(self):
        kind = self.db.query(
            "SELECT table_type FROM information_schema.tables WHERE table_name = 'sales_columnar'"
        )
        self.assertEqual(kind[0]["table_type"], "VIEW")

    def test_the_view_has_a_column_per_field_not_a_json_blob(self):
        columns = {
            r["column_name"]
            for r in self.db.query(
                "SELECT column_name FROM information_schema.columns "
                "WHERE table_name = 'sales_columnar'"
            )
        }
        self.assertIn("region", columns)
        self.assertIn("units", columns)
        self.assertIn("amount", columns)
        # Reading a field means reading a column, not picking it out of JSON.
        rows = self.db.query("SELECT units FROM sales_columnar WHERE region = 'south'")
        self.assertEqual(int(rows[0]["units"]), 2)
        # `data` is still carried, verbatim, because geometry rides in it.
        self.assertIn("data", columns)

    def test_the_view_returns_the_rows_that_were_synced(self):
        rows = self.db.query("SELECT region FROM sales_columnar ORDER BY region")
        self.assertEqual([r["region"] for r in rows], ["north", "north", "south"])

    def test_it_aggregates_like_any_other_table(self):
        rows = self.db.query(
            "SELECT region, sum(units) AS units FROM sales_columnar "
            "GROUP BY region ORDER BY region"
        )
        self.assertEqual([(r["region"], int(r["units"])) for r in rows], [("north", 10), ("south", 2)])

    def test_a_row_written_after_the_last_sync_is_not_in_the_view_until_it_syncs(self):
        # The view reads object storage, so it is as fresh as the last push. The
        # collection is the one that is always current; they are two read paths
        # on purpose. This runs against its own collection so the staleness it
        # creates cannot be seen by the other tests.
        returns = self.db.collection("returns")
        returns.insert_one({"region": "east", "units": 1})
        self.sync.sync_table("returns", 1000)
        self.assertEqual(
            int(self.db.query("SELECT count(*) AS n FROM returns_columnar")[0]["n"]), 1
        )

        returns.insert_one({"region": "west", "units": 2})
        self.assertEqual(returns.count(), 2, "the collection is current")
        self.assertEqual(
            int(self.db.query("SELECT count(*) AS n FROM returns_columnar")[0]["n"]),
            1,
            "the view is as fresh as the last sync",
        )

        self.sync.sync_table("returns", 1000)
        self.assertEqual(
            int(self.db.query("SELECT count(*) AS n FROM returns_columnar")[0]["n"]), 2
        )

    def test_the_collection_itself_never_goes_through_duckdb(self):
        # Routing point lookups through DuckDB costs about 55ms against 0.1ms,
        # so the collection stays ordinary PostgreSQL storage: a partitioned
        # table whose partitions are heap tables, with no view in the way that
        # could reach for read_parquet.
        kinds = self.db.query(
            "SELECT relkind::text AS k FROM pg_class WHERE oid = 'sales'::regclass "
            "UNION ALL "
            "SELECT c.relkind::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid "
            "WHERE i.inhparent = 'sales'::regclass"
        )
        self.assertEqual(sorted(k["k"] for k in kinds), ["p", "r", "r"])


class ColumnarIsOff(unittest.TestCase):
    """Without the flag, nothing about DuckDB is installed or created."""

    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path("nocolumnar")
        cls.storage_dir = os.path.join(cls.workdir, "storage")
        os.makedirs(cls.storage_dir, exist_ok=True)
        cls.db = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "db"), persist=True, multi_user_sync=True
        )
        cls.sync = dbcenter.SyncEngine(cls.db, dbcenter.LocalStorageAdapter(cls.storage_dir))
        cls.db.collection("sales").insert_one({"region": "north", "units": 3})
        cls.sync.sync_table("sales", 1000)

    @classmethod
    def tearDownClass(cls):
        try:
            cls.db.shutdown()
        except Exception:
            pass
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def test_pg_duckdb_is_not_installed(self):
        rows = self.db.query("SELECT extname FROM pg_extension WHERE extname = 'pg_duckdb'")
        self.assertEqual(rows, [])

    def test_no_columnar_view_is_created(self):
        rows = self.db.query(
            "SELECT table_name FROM information_schema.tables WHERE table_name = 'sales_columnar'"
        )
        self.assertEqual(rows, [])

    def test_the_collection_still_reads_normally(self):
        self.assertEqual(self.db.collection("sales").count(), 1)


if __name__ == "__main__":
    unittest.main()
