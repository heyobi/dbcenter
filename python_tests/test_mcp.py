"""The MCP server, and the read-only guarantee it rests on.

Read-only is the server's default and the reason it is safe to hand to a
language model, so it is tested against the database rather than the SQL text:
each case is something a keyword filter would miss, and PostgreSQL has to
refuse every one of them.
"""

import asyncio
import os
import sys
import unittest

import dbcenter

from harness import DBTestCase, temp_path

try:
    from mcp.server.mcpserver.exceptions import ToolError

    from dbcenter.mcp_server import build_server

    HAVE_MCP = True
except ImportError:  # the mcp extra is optional, and needs Python 3.10
    HAVE_MCP = False


class ReadOnlyQuery(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.db.collection("items").insert_many([{"n": 1}, {"n": 2}])
        cls.db.query("CREATE SEQUENCE IF NOT EXISTS ticket")

    def test_a_select_works(self):
        rows = self.db.query("SELECT sum(n) AS s FROM items", read_only=True)
        self.assertEqual(rows[0]["s"], 3)

    def test_every_way_of_writing_is_refused(self):
        for sql in (
            "INSERT INTO items (n) VALUES (3)",
            "WITH gone AS (DELETE FROM items RETURNING *) SELECT count(*) FROM gone",
            "/* SELECT */ UPDATE items SET n = 0",
            "SELECT nextval('ticket')",
            "CREATE TABLE sneaky (x int)",
            "DROP TABLE items",
        ):
            with self.subTest(sql=sql):
                with self.assertRaisesRegex(Exception, "read-only transaction"):
                    self.db.query(sql, read_only=True)
        self.assertEqual(self.db.collection("items").count(), 2)


def call(server, tool, **arguments):
    result = asyncio.run(server.call_tool(tool, arguments))
    content = result.structured_content
    if isinstance(content, dict) and set(content) == {"result"}:
        return content["result"]
    return content


def tool_names(server):
    return sorted(t.name for t in asyncio.run(server.list_tools()))


@unittest.skipUnless(HAVE_MCP, "the mcp extra is not installed")
class ReadOnlyServer(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.db.collection("cities").insert_many(
            [{"name": "Istanbul", "pop": 15}, {"name": "Ankara", "pop": 5}]
        )
        cls.server = build_server(cls.db)

    def test_only_reading_tools_exist(self):
        self.assertEqual(
            tool_names(self.server),
            ["count", "describe_collection", "find", "list_collections", "sql"],
        )

    def test_find_count_and_describe(self):
        self.assertIn("cities", call(self.server, "list_collections"))
        found = call(self.server, "find", collection_name="cities", query={"pop": {"$gt": 10}})
        self.assertEqual([d["name"] for d in found["documents"]], ["Istanbul"])
        self.assertEqual(call(self.server, "count", collection_name="cities"), 2)
        described = call(self.server, "describe_collection", name="cities")
        self.assertEqual(described["count"], 2)
        self.assertIn("pop", [f["name"] for f in described["fields"]])

    def test_sql_reads_and_cannot_write(self):
        rows = call(self.server, "sql", query="SELECT max(pop) AS m FROM cities")["rows"]
        self.assertEqual(rows[0]["m"], 15)
        with self.assertRaisesRegex(ToolError, "read-only transaction"):
            call(self.server, "sql", query="DELETE FROM cities")
        self.assertEqual(call(self.server, "count", collection_name="cities"), 2)

    def test_a_missing_collection_is_named_not_created(self):
        with self.assertRaisesRegex(ToolError, "No collection named 'citiez'"):
            call(self.server, "find", collection_name="citiez")
        self.assertNotIn("citiez", self.db.list_collections())


@unittest.skipUnless(HAVE_MCP, "the mcp extra is not installed")
class WritingServer(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.server = build_server(cls.db, allow_writes=True)

    def test_writes_round_trip(self):
        self.assertIn("insert", tool_names(self.server))
        ids = call(
            self.server, "insert", collection_name="notes", documents=[{"t": "a"}, {"t": "b"}]
        )
        self.assertEqual(len(ids), 2)
        changed = call(
            self.server, "update", collection_name="notes",
            query={"t": "a"}, update={"$set": {"t": "A"}},
        )
        self.assertEqual(changed, 1)
        with self.assertRaisesRegex(ToolError, "empty query"):
            call(self.server, "delete", collection_name="notes", query={}, many=True)
        self.assertEqual(call(self.server, "count", collection_name="notes"), 2)
        self.assertEqual(call(self.server, "delete", collection_name="notes", query={"t": "b"}), 1)
        self.assertEqual(call(self.server, "count", collection_name="notes"), 1)


@unittest.skipUnless(HAVE_MCP, "the mcp extra is not installed")
class OverStdio(unittest.TestCase):
    """The way a client actually starts it: a process speaking MCP on stdio."""

    def test_a_client_can_list_and_call_tools(self):
        from mcp.client.session import ClientSession
        from mcp.client.stdio import StdioServerParameters, stdio_client

        path = temp_path("mcp-stdio")
        params = StdioServerParameters(
            command=sys.executable,
            args=["-m", "dbcenter.mcp_server", "--path", path],
            env=dict(os.environ),
        )

        async def session():
            async with stdio_client(params) as (read, write):
                async with ClientSession(read, write) as client:
                    await client.initialize()
                    tools = await client.list_tools()
                    result = await client.call_tool("sql", {"query": "SELECT 41 + 1 AS n"})
                    refused = await client.call_tool("sql", {"query": "CREATE TABLE t (x int)"})
                    return tools, result, refused

        tools, result, refused = asyncio.run(session())
        self.assertIn("sql", [t.name for t in tools.tools])
        self.assertFalse(result.is_error)
        self.assertEqual(result.structured_content["rows"][0]["n"], 42)
        self.assertTrue(refused.is_error)
        self.assertIn("read-only transaction", refused.content[0].text)


if __name__ == "__main__":
    unittest.main()
