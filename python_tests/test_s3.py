"""Syncing to an S3-compatible bucket from Python.

Runs only when `DBCENTER_S3_TEST_ENDPOINT` is set, like the Rust S3 tests, so a
machine without object storage still passes. `scripts/s3-test-env.sh` points it
at a local MinIO.
"""

import os
import unittest
import uuid

import dbcenter

from harness import temp_path

ENDPOINT = os.environ.get("DBCENTER_S3_TEST_ENDPOINT")


def bucket(prefix):
    return dbcenter.S3StorageAdapter(
        os.environ.get("DBCENTER_S3_TEST_BUCKET", "dbcenter-test"),
        endpoint=ENDPOINT,
        access_key=os.environ.get("DBCENTER_S3_TEST_KEY", "dbcenter"),
        secret_key=os.environ.get("DBCENTER_S3_TEST_SECRET", "dbcenter123"),
        region="us-east-1",
        prefix=prefix,
    )


class Arguments(unittest.TestCase):
    def test_one_key_without_the_other_is_refused(self):
        with self.assertRaisesRegex(ValueError, "both access_key and secret_key"):
            dbcenter.S3StorageAdapter("b", access_key="only-this")


@unittest.skipUnless(ENDPOINT, "DBCENTER_S3_TEST_ENDPOINT is not set")
class RoundTrip(unittest.TestCase):
    def test_a_table_synced_to_s3_rebuilds_on_another_database(self):
        # Its own prefix: the bucket outlives the test, and a previous run's
        # files would otherwise be restored too.
        prefix = f"py-{uuid.uuid4().hex[:8]}"

        a = dbcenter.DBCenter(path=temp_path("s3-a"), multi_user_sync=True)
        try:
            a.collection("notes").insert_many([{"body": "one"}, {"body": "two"}])
            self.assertEqual(dbcenter.SyncEngine(a, bucket(prefix)).sync_table("notes", 100), 2)
        finally:
            a.shutdown()

        self.assertTrue(bucket(prefix).list("notes"))

        b = dbcenter.DBCenter(path=temp_path("s3-b"), multi_user_sync=True)
        try:
            dbcenter.SyncEngine(b, bucket(prefix)).pull_and_apply("notes")
            rows = b.query("SELECT body FROM notes ORDER BY body")
            self.assertEqual([r["body"] for r in rows], ["one", "two"])
        finally:
            b.shutdown()


if __name__ == "__main__":
    unittest.main()
