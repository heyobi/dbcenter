"""The pytest fixtures in dbcenter.testing, run by pytest itself.

They are loaded through the pytest11 entry point, so the sample tests below
use them without any conftest - which is what a user gets from installing
dbcenter.
"""

import os
import subprocess
import sys
import textwrap
import unittest

from harness import DBTestCase, temp_path

try:
    import pytest  # noqa: F401

    HAVE_PYTEST = True
except ImportError:
    HAVE_PYTEST = False

SAMPLE = textwrap.dedent(
    """
    def test_first_creates_a_table(dbcenter_db):
        dbcenter_db.query("CREATE TABLE leftovers (x int)")
        dbcenter_db.query("INSERT INTO leftovers VALUES (1)")
        assert dbcenter_db.query("SELECT count(*) AS n FROM leftovers")[0]["n"] == 1

    def test_second_does_not_see_it(dbcenter_db):
        rows = dbcenter_db.query(
            "SELECT count(*) AS n FROM information_schema.tables "
            "WHERE table_schema = 'public' AND table_name = 'leftovers'"
        )
        assert rows[0]["n"] == 0

    def test_connection_details(postgres_dsn, postgres_url):
        assert "port=" in postgres_dsn and "user=" in postgres_dsn
        assert postgres_url.startswith("postgresql://")
    """
)


@unittest.skipUnless(HAVE_PYTEST, "pytest is not installed")
class Fixtures(unittest.TestCase):
    def test_the_fixtures_load_and_isolate_tests(self):
        workdir = temp_path("pytest-plugin")
        os.makedirs(workdir)
        with open(os.path.join(workdir, "test_sample.py"), "w") as f:
            f.write(SAMPLE)
        run = subprocess.run(
            [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", workdir],
            capture_output=True,
            text=True,
            cwd=workdir,
        )
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("3 passed", run.stdout)


class ConnectionDetails(DBTestCase):
    def test_dsn_and_url_name_this_server(self):
        port = str(self.db.port)
        self.assertIn(f"port={port}", self.db.dsn)
        self.assertIn("dbname=postgres", self.db.dsn)
        url = self.db.url
        self.assertTrue(url.startswith("postgresql://postgres@"), url)
        if "?host=" in url:  # Unix socket: the directory and port go in the query
            self.assertIn(f"port={port}", url)
        else:
            self.assertIn(f":{port}/", url)


if __name__ == "__main__":
    unittest.main()
