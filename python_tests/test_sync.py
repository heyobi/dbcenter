"""The delta layer and the round trip through object storage.

This is where the data-loss bugs were: a synced row lost every field but `id`,
an updated row disappeared from the collection, and a compaction replaced the
deltas with an empty base. Nothing here checks that a call returned - each one
checks what came back out the other side.
"""

import glob
import json
import os
import shutil
import unittest

import dbcenter

from harness import DBTestCase, temp_path


class DeltaLayer(DBTestCase):
    db_kwargs = {"multi_user_sync": True}

    def setUp(self):
        self.storage_dir = os.path.join(self.workdir, f"storage-{id(self)}")
        os.makedirs(self.storage_dir, exist_ok=True)
        self.storage = dbcenter.LocalStorageAdapter(self.storage_dir)
        self.sync = dbcenter.SyncEngine(self.db, self.storage)
        self.table, self.orders = self.new_collection()

    def objects(self):
        return sorted(os.path.basename(p) for p in glob.glob(os.path.join(self.storage_dir, "**", "*.parquet"), recursive=True))

    def test_the_collection_reads_through_the_three_delta_tables(self):
        self.orders.insert_one({"ref": "A-1", "total": 10.5})
        # A view over base/adds/dels, not a table.
        kind = self.db.query(
            "SELECT table_type FROM information_schema.tables WHERE table_name = $1",
            [self.table],
        )
        self.assertEqual(kind[0]["table_type"], "VIEW")
        for suffix in ("base", "adds", "dels"):
            rows = self.db.query(
                "SELECT count(*) AS n FROM information_schema.tables WHERE table_name = $1",
                [f"{self.table}_{suffix}"],
            )
            self.assertEqual(rows[0]["n"], 1, f"{self.table}_{suffix} is missing")

    def test_an_insert_lands_in_adds_and_a_sync_moves_it_to_base(self):
        self.orders.insert_one({"ref": "A-1", "total": 10.5})
        self.assertEqual(self.db.query(f'SELECT count(*) AS n FROM "{self.table}_adds"')[0]["n"], 1)

        moved = self.sync.sync_table(self.table, 1000)
        self.assertEqual(moved, 1)
        self.assertEqual(self.db.query(f'SELECT count(*) AS n FROM "{self.table}_adds"')[0]["n"], 0)
        self.assertEqual(self.db.query(f'SELECT count(*) AS n FROM "{self.table}_base"')[0]["n"], 1)
        # And the row is still there when read through the collection.
        self.assertEqual(self.orders.count(), 1)

    def test_a_sync_writes_a_parquet_object(self):
        self.orders.insert_many([{"ref": f"A-{i}", "total": float(i)} for i in range(5)])
        self.sync.sync_table(self.table, 1000)
        written = self.objects()
        self.assertEqual(len(written), 1, f"expected one object, got {written}")
        self.assertIn("_DELTA_", written[0])

    def test_a_second_sync_with_nothing_new_writes_nothing(self):
        self.orders.insert_one({"ref": "A-1"})
        self.sync.sync_table(self.table, 1000)
        after_first = self.objects()

        self.assertEqual(self.sync.sync_table(self.table, 1000), 0)
        self.assertEqual(self.objects(), after_first, "an empty sync wrote an object")

    def test_an_updated_row_survives_the_sync(self):
        # The update writes a tombstone into _dels and a new row into _adds. The
        # sync used to move the row into _base and leave the tombstone behind,
        # so the view subtracted the row it had just written and the document
        # vanished from the collection entirely.
        self.orders.insert_one({"ref": "A-1", "total": 10.0})
        self.sync.sync_table(self.table, 1000)
        self.orders.update_one({"ref": "A-1"}, {"$set": {"total": 99.0}})
        self.sync.sync_table(self.table, 1000)

        self.assertEqual(self.orders.count(), 1, "the updated row is gone")
        self.assertEqual(self.orders.find_one({"ref": "A-1"})["total"], 99.0)

    def test_a_deleted_row_stays_deleted_across_a_sync(self):
        self.orders.insert_many([{"ref": "A-1"}, {"ref": "A-2"}])
        self.sync.sync_table(self.table, 1000)
        self.orders.delete_one({"ref": "A-1"})
        self.assertEqual(self.orders.count(), 1)
        self.sync.sync_table(self.table, 1000)
        self.assertEqual([d["ref"] for d in self.orders.find()], ["A-2"])

    def test_a_vacuum_replaces_the_deltas_with_one_base_object(self):
        self.orders.insert_many([{"ref": f"A-{i}"} for i in range(10)])
        self.sync.sync_table(self.table, 1000)
        self.sync.vacuum(self.table)

        written = self.objects()
        self.assertTrue(any("_BASE_" in name for name in written), written)
        # The rows are still readable, which is what the empty-base bug broke.
        self.assertEqual(self.orders.count(), 10)


class TwoNodes(unittest.TestCase):
    """One node writes, another reads it back out of object storage."""

    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path("twonodes")
        cls.storage_dir = os.path.join(cls.workdir, "storage")
        os.makedirs(cls.storage_dir, exist_ok=True)
        cls.writer = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "writer"), persist=True, multi_user_sync=True
        )
        cls.reader = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "reader"), persist=True, multi_user_sync=True
        )
        cls.write_sync = dbcenter.SyncEngine(cls.writer, dbcenter.LocalStorageAdapter(cls.storage_dir))
        cls.read_sync = dbcenter.SyncEngine(cls.reader, dbcenter.LocalStorageAdapter(cls.storage_dir))

        cls.orders = cls.writer.collection("orders")
        cls.orders.insert_many(
            [
                {"ref": "A-1", "qty": 3, "total": 10.5, "paid": True},
                {"ref": "A-2", "qty": 7, "total": 20.25, "paid": False},
            ]
        )
        cls.write_sync.sync_table("orders", 1000)

    @classmethod
    def tearDownClass(cls):
        for db in (cls.writer, cls.reader):
            try:
                db.shutdown()
            except Exception:
                pass
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def test_the_reader_sees_the_rows_the_writer_pushed(self):
        self.read_sync.pull_and_apply("orders", False)
        rows = self.reader.collection("orders").find()
        self.assertEqual(sorted(r["ref"] for r in rows), ["A-1", "A-2"])

    def test_every_field_survives_the_trip_with_its_type(self):
        # A synced row used to arrive with `id` and nothing else, because the
        # Parquet held one JSON blob and three code paths read it by position.
        self.read_sync.pull_and_apply("orders", False)
        row = self.reader.collection("orders").find_one({"ref": "A-2"})
        self.assertIsNotNone(row, "the row did not arrive at all")
        self.assertEqual(row["qty"], 7)
        self.assertIsInstance(row["qty"], int)
        self.assertAlmostEqual(row["total"], 20.25, places=6)
        self.assertIsInstance(row["total"], float)
        self.assertIs(row["paid"], False)

    def test_pulling_twice_does_not_double_the_rows(self):
        self.read_sync.pull_and_apply("orders", False)
        self.read_sync.pull_and_apply("orders", False)
        self.assertEqual(self.reader.collection("orders").count(), 2)


class SmartQuery(unittest.TestCase):
    """The point lookup that needs no extension.

    `smart_query` reads Parquet over range requests and skips row groups whose
    id range cannot hold the target. The columnar view is faster and does
    arbitrary SQL, but it needs pg_duckdb; this path works everywhere.
    """

    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path("smartquery")
        cls.storage_dir = os.path.join(cls.workdir, "storage")
        os.makedirs(cls.storage_dir, exist_ok=True)
        writer = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "writer"), persist=True, multi_user_sync=True
        )
        sync = dbcenter.SyncEngine(writer, dbcenter.LocalStorageAdapter(cls.storage_dir))
        notes = writer.collection("notes")
        cls.ids = notes.insert_many(
            [{"content": f"note {i}", "n": i} for i in range(500)]
        )
        sync.sync_table("notes", 1000)
        sync.vacuum("notes")
        writer.shutdown()
        del sync
        del notes
        del writer

        cls.reader_path = os.path.join(cls.workdir, "reader")
        cls.reader = dbcenter.DBCenter(path=cls.reader_path, persist=True, multi_user_sync=True)
        cls.read_sync = dbcenter.SyncEngine(cls.reader, dbcenter.LocalStorageAdapter(cls.storage_dir))

    @classmethod
    def tearDownClass(cls):
        try:
            cls.reader.shutdown()
        except Exception:
            pass
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def test_a_lookup_returns_the_document_that_was_written(self):
        target = self.ids[250]
        hits = self.read_sync.smart_query("notes", target)
        self.assertEqual(len(hits), 1, f"expected one hit for {target}, got {hits}")
        found = json.loads(hits[0])
        self.assertEqual(found["content"], "note 250")
        self.assertEqual(found["n"], 250)

    def test_an_id_that_was_never_written_returns_nothing(self):
        self.assertEqual(self.read_sync.smart_query("notes", "00000000-0000-0000-0000-000000000000"), [])

    def test_the_footer_is_cached_after_the_first_lookup(self):
        # The cache used to be written by a rule Parquet never satisfies, so the
        # file was never created and every lookup re-read the footer.
        self.read_sync.smart_query("notes", self.ids[10])
        meta_dir = os.path.join(self.reader_path, ".meta")
        cached = os.listdir(meta_dir)
        self.assertTrue(cached, "no footer was cached")
        self.assertTrue(all(name.endswith(".meta") for name in cached), cached)
        for name in cached:
            self.assertGreater(os.path.getsize(os.path.join(meta_dir, name)), 8)

    def test_a_second_lookup_still_finds_the_right_row(self):
        # Once the footer comes off disk, the answer has to stay the same.
        first = self.read_sync.smart_query("notes", self.ids[42])
        second = self.read_sync.smart_query("notes", self.ids[42])
        self.assertEqual(first, second)
        self.assertEqual(json.loads(second[0])["n"], 42)

    def test_the_reader_never_had_the_rows_locally(self):
        # The point of the lookup: it answers without a copy of the table. The
        # reader never created the collection, so the tables do not even exist.
        present = self.reader.query(
            "SELECT count(*) AS n FROM information_schema.tables WHERE table_name LIKE 'notes%'"
        )
        self.assertEqual(present[0]["n"], 0)


if __name__ == "__main__":
    unittest.main()
