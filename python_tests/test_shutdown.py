"""shutdown() stops the server before it returns.

It used to hand the stopping to the watcher, which did it after the call had
returned and, when the server was slow to go, with SIGKILL. A script that
reopened the same directory straight away - the next cell of a notebook, the
next run of a cron job - found the old server still up and was refused with
"PostgreSQL is already running".
"""

import os
import subprocess
import sys
import unittest

from harness import temp_path

SCRIPT = """
import sys, dbcenter
db = dbcenter.DBCenter(path=sys.argv[1])
db.query("CREATE TABLE IF NOT EXISTS runs (n int)")
db.query("INSERT INTO runs VALUES (1)")
print(db.query("SELECT count(*) AS n FROM runs")[0]["n"])
db.shutdown()
"""


class Shutdown(unittest.TestCase):
    def test_a_directory_can_be_reopened_as_soon_as_the_last_process_shut_down(self):
        path = temp_path("reopen")
        counts = []
        for _ in range(3):
            run = subprocess.run(
                [sys.executable, "-c", SCRIPT, path], capture_output=True, text=True
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            counts.append(int(run.stdout.strip().splitlines()[-1]))
            # Stopped, not killed: a clean stop removes the pid file.
            self.assertFalse(
                os.path.exists(os.path.join(path, "postmaster.pid")),
                "the server was still up, or was killed, after shutdown() returned",
            )
        self.assertEqual(counts, [1, 2, 3])

    def test_the_context_manager_stops_it_too(self):
        path = temp_path("with")
        code = (
            "import sys, dbcenter\n"
            "with dbcenter.DBCenter(path=sys.argv[1]) as db:\n"
            "    db.query('SELECT 1')\n"
        )
        for _ in range(2):
            run = subprocess.run(
                [sys.executable, "-c", code, path], capture_output=True, text=True
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertFalse(os.path.exists(os.path.join(path, "postmaster.pid")))


if __name__ == "__main__":
    unittest.main()
