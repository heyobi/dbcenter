"""Shared setup for the Python tests.

Every test class gets its own database directory and its own PostgreSQL, which
costs a few seconds per class and buys isolation: a test that corrupts a table
cannot make a later one fail for a reason that has nothing to do with it.
"""

import os
import shutil
import tempfile
import unittest
import uuid

import dbcenter


def temp_path(label):
    """A directory nothing else is using, under the system temp dir."""
    return os.path.join(
        tempfile.gettempdir(), "dbcenter-tests", f"{label}-{uuid.uuid4().hex[:8]}"
    )


class DBTestCase(unittest.TestCase):
    """A test class with a live database.

    Set `db_kwargs` to change how it is built - `multi_user_sync=True` for the
    delta layer, `columnar=True` for the pg_duckdb view.
    """

    db_kwargs = {}

    @classmethod
    def setUpClass(cls):
        cls.workdir = temp_path(cls.__name__.lower())
        os.makedirs(cls.workdir, exist_ok=True)
        cls.db_path = os.path.join(cls.workdir, "db")
        cls.db = dbcenter.DBCenter(path=cls.db_path, persist=True, **cls.db_kwargs)

    @classmethod
    def tearDownClass(cls):
        try:
            cls.db.shutdown()
        except Exception:
            pass
        del cls.db
        shutil.rmtree(cls.workdir, ignore_errors=True)

    def new_collection(self):
        """A collection nobody else in this class is writing to.

        Returns the name alongside it: `Collection.__getattr__` reads any
        unknown attribute as a column, so asking a collection for its own
        `.name` would hand back the values of a field called "name".
        """
        name = f"c_{uuid.uuid4().hex[:8]}"
        return name, self.db.collection(name)
