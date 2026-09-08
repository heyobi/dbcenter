"""The Google Drive adapter, against a real Drive.

Nothing here can run unattended: the adapter opens an OAuth consent flow the
first time. It is skipped unless DBCENTER_DRIVE_CLIENT_SECRET points at a
client-secret JSON file, which is also why the credential is not in this
repository - the previous version of this file carried one inline.

    DBCENTER_DRIVE_CLIENT_SECRET=~/client_secret.json python -m unittest test_drive
"""

import json
import os
import shutil
import unittest

import dbcenter

from harness import temp_path

CLIENT_SECRET = os.environ.get("DBCENTER_DRIVE_CLIENT_SECRET")
ROOT_FOLDER = os.environ.get("DBCENTER_DRIVE_FOLDER", "dbcenter_test_store")


@unittest.skipUnless(CLIENT_SECRET, "set DBCENTER_DRIVE_CLIENT_SECRET to run this")
class DriveRoundTrip(unittest.TestCase):
    """Write on one node, read the row back on another that has no local copy."""

    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path("drive")
        os.makedirs(cls.workdir, exist_ok=True)
        writer = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "writer"), persist=True, multi_user_sync=True
        )
        storage = dbcenter.GoogleDriveAdapter(os.path.expanduser(CLIENT_SECRET), ROOT_FOLDER)
        sync = dbcenter.SyncEngine(writer, storage)

        users = writer.collection("drive_users")
        cls.ids = users.insert_many(
            [{"name": f"User {i}", "value": i * 10} for i in range(100)]
        )
        sync.sync_table("drive_users", 1000)
        sync.vacuum("drive_users")
        cls.written = storage.list("drive_users")

        writer.shutdown()
        del sync, users, writer

        cls.reader = dbcenter.DBCenter(
            path=os.path.join(cls.workdir, "reader"), persist=True, multi_user_sync=True
        )
        cls.read_sync = dbcenter.SyncEngine(
            cls.reader,
            dbcenter.GoogleDriveAdapter(os.path.expanduser(CLIENT_SECRET), ROOT_FOLDER),
        )

    @classmethod
    def tearDownClass(cls):
        try:
            cls.reader.shutdown()
        except Exception:
            pass
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def test_the_sync_left_parquet_on_drive(self):
        self.assertTrue(
            any(name.endswith(".parquet") for name in self.written), self.written
        )

    def test_a_cold_node_can_look_one_row_up_over_range_requests(self):
        found = self.read_sync.smart_query("drive_users", self.ids[50])
        self.assertEqual(len(found), 1, found)
        self.assertEqual(json.loads(found[0])["name"], "User 50")


if __name__ == "__main__":
    unittest.main()
