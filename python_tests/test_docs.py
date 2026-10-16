"""The examples in the README and in llms.txt, run as written.

Documentation that nobody executes drifts from the code. The first draft of
the quick start used `$inc`, which does not exist, and the README's Go example
imported a module path that has never existed. So every Python block that
starts with `import dbcenter` or `from dbcenter import` runs here, in a
directory of its own, and the claims the examples cannot show are checked one
by one below.
"""

import os
import re
import unittest

import dbcenter

from harness import DBTestCase, temp_path

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def examples(filename):
    with open(os.path.join(REPO, filename), encoding="utf-8") as f:
        text = f.read()
    blocks = re.findall(r"```python\n(.*?)```", text, re.S)
    runnable = ("import dbcenter", "from dbcenter import")
    return [b for b in blocks if b.startswith(runnable)]


class Examples(unittest.TestCase):
    def run_examples(self, filename):
        found = examples(filename)
        self.assertTrue(found, f"no runnable examples in {filename}")
        for i, code in enumerate(found):
            with self.subTest(file=filename, example=i):
                workdir = temp_path(f"docs-{i}")
                os.makedirs(workdir)
                here = os.getcwd()
                os.chdir(workdir)
                try:
                    exec(compile(code, f"{filename} example {i}", "exec"), {})
                finally:
                    os.chdir(here)

    def test_readme(self):
        self.run_examples("README.md")

    def test_llms_txt(self):
        self.run_examples("llms.txt")

    def test_the_pypi_readme(self):
        self.run_examples(os.path.join("dbcenter-python", "README.md"))


class Claims(DBTestCase):
    """What llms.txt says that no example demonstrates."""

    def test_only_set_and_plain_values_update(self):
        name, c = self.new_collection()
        c.insert_one({"k": "a", "n": 1})
        c.update_one({"k": "a"}, {"n": 2})
        self.assertEqual(c.find_one({"k": "a"})["n"], 2)
        for op in ("$inc", "$unset", "$push"):
            with self.subTest(op=op), self.assertRaises(Exception):
                c.update_one({"k": "a"}, {op: {"n": 1}})

    def test_an_unknown_query_operator_raises(self):
        name, c = self.new_collection()
        c.insert_one({"age": 1})
        with self.assertRaises(Exception):
            c.delete_many({"age": {"$typo": 1}})
        self.assertEqual(c.count(), 1)

    def test_find_has_ids_both_ways(self):
        name, c = self.new_collection()
        c.insert_one({"x": 1})
        doc = c.find_one()
        self.assertEqual(doc["id"], doc["_id"])

    def test_query_binds_dollar_parameters(self):
        self.assertEqual(self.db.query("SELECT $1::int + 1 AS n", [41])[0]["n"], 42)

    def test_delete_many_without_a_query_deletes_everything(self):
        name, c = self.new_collection()
        c.insert_many([{"x": i} for i in range(3)])
        self.assertEqual(c.delete_many(), 3)
        self.assertEqual(c.count(), 0)


if __name__ == "__main__":
    unittest.main()
