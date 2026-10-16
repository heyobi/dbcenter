"""An MCP server over a DBCenter database, so a language model can use it.

    pip install "dbcenter[mcp]"
    dbcenter-mcp --path ./data                  # read-only
    dbcenter-mcp --path ./data --allow-writes   # and insert/update/delete

It speaks MCP over stdio, which is how desktop clients and coding agents start
a local server. The server owns the database: it starts PostgreSQL on the
directory given and stops it on exit, so nothing else may have that directory
open at the same time.

Read-only is the default, and it is enforced by PostgreSQL rather than by
looking at the SQL: the `sql` tool runs each statement in a READ ONLY
transaction that is rolled back, so a write is refused however it is phrased.
The write tools do not exist at all unless `--allow-writes` is given, and even
then `delete` refuses an empty query - `delete_many({})` empties a collection,
which is not something a model should be one argument away from.
"""

import argparse
import atexit
import functools
from typing import Any, Optional

from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError
from mcp.types import ToolAnnotations

import dbcenter

# More rows than this in one answer floods a model's context for no benefit;
# the caller is told when an answer was cut short.
MAX_ROWS = 200

INSTRUCTIONS = """\
A DBCenter database: real PostgreSQL with MongoDB-style collections. Each
collection is a table with one column per document field.

Start with list_collections and describe_collection. find and count take a
MongoDB-style query: {"field": value} or {"field": {"$gt": 1}}, with $eq $ne
$gt $gte $lt $lte $in $nin $exists $regex. For ordering, joins, grouping or
anything else, use sql, which is ordinary PostgreSQL with $1, $2 parameters.
"""

READ = ToolAnnotations(read_only_hint=True, open_world_hint=False)
WRITE = ToolAnnotations(read_only_hint=False, destructive_hint=False, open_world_hint=False)
DESTROY = ToolAnnotations(read_only_hint=False, destructive_hint=True, open_world_hint=False)


def _quote(identifier: str) -> str:
    return '"' + identifier.replace('"', '""') + '"'


def _reported(fn):
    """Let the model read why a tool failed.

    MCP reports an unexpected exception as "Error executing tool sql" and no
    more, which leaves the model guessing. A ToolError's message reaches it, so
    "cannot execute INSERT in a read-only transaction" or "No collection named
    'orderz'" arrives as said and the model can correct itself.
    """

    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        try:
            return fn(*args, **kwargs)
        except ToolError:
            raise
        except Exception as exc:
            raise ToolError(str(exc)) from exc

    return wrapper


def _capped(rows: list) -> dict:
    return {
        "rows": rows[:MAX_ROWS],
        "returned": min(len(rows), MAX_ROWS),
        "truncated": len(rows) > MAX_ROWS,
    }


def build_server(db: "dbcenter.DBCenter", allow_writes: bool = False) -> MCPServer:
    """The MCP server for `db`. Separate from main() so tests can drive it."""
    mode = "read and write" if allow_writes else "read-only"
    server = MCPServer(
        name="dbcenter",
        instructions=INSTRUCTIONS + f"\nThis server is {mode}.",
    )

    def collection(name: str) -> "dbcenter.Collection":
        # Only names that exist: db.collection() would otherwise create a
        # table for any string it is handed.
        if name not in db.list_collections():
            raise ValueError(
                f"No collection named {name!r}. Collections: {db.list_collections()}"
            )
        return db.collection(name)

    @server.tool(annotations=READ)
    @_reported
    def list_collections() -> list[str]:
        """The collections in this database."""
        return db.list_collections()

    @server.tool(annotations=READ)
    @_reported
    def describe_collection(name: str) -> dict[str, Any]:
        """A collection's fields with their PostgreSQL types, and its row count."""
        collection(name)
        columns = db.query(
            "SELECT column_name AS name, data_type AS type "
            "FROM information_schema.columns "
            "WHERE table_schema = current_schema() AND table_name = $1 "
            "AND column_name <> '_layer' ORDER BY ordinal_position",
            [name],
            read_only=True,
        )
        count = db.query(f"SELECT count(*) AS n FROM {_quote(name)}", read_only=True)
        return {"name": name, "fields": columns, "count": count[0]["n"]}

    @server.tool(annotations=READ)
    @_reported
    def find(
        collection_name: str,
        query: Optional[dict[str, Any]] = None,
        limit: int = 50,
    ) -> dict[str, Any]:
        """Documents matching a MongoDB-style query, at most `limit` of them."""
        docs = collection(collection_name).find(query or None)
        limit = max(1, min(limit, MAX_ROWS))
        return {
            "documents": docs[:limit],
            "returned": min(len(docs), limit),
            "total_matching": len(docs),
        }

    @server.tool(annotations=READ)
    @_reported
    def count(collection_name: str, query: Optional[dict[str, Any]] = None) -> int:
        """How many documents match a MongoDB-style query."""
        return collection(collection_name).count(query or None)

    @server.tool(annotations=READ)
    @_reported
    def sql(query: str, params: Optional[list[Any]] = None) -> dict[str, Any]:
        """Run one PostgreSQL statement read-only, with $1, $2 parameters.

        It runs in a READ ONLY transaction that is rolled back, so anything
        that would change the database is refused by PostgreSQL.
        """
        return _capped(db.query(query, params or None, read_only=True))

    if not allow_writes:
        return server

    @server.tool(annotations=WRITE)
    @_reported
    def insert(collection_name: str, documents: list[dict[str, Any]]) -> list[str]:
        """Insert documents; a new collection is created on first insert.

        Returns their ids. Each new field becomes a column, typed from its
        first value.
        """
        return db.collection(collection_name).insert_many(documents)

    @server.tool(annotations=WRITE)
    @_reported
    def update(
        collection_name: str,
        query: dict[str, Any],
        update: dict[str, Any],
        many: bool = False,
    ) -> int:
        """Update the first matching document, or every one with many=True.

        `update` is {"$set": {...}} or a plain {field: value} map.
        Returns how many documents changed.
        """
        c = collection(collection_name)
        return c.update_many(query, update) if many else c.update_one(query, update)

    @server.tool(annotations=DESTROY)
    @_reported
    def delete(collection_name: str, query: dict[str, Any], many: bool = False) -> int:
        """Delete the first matching document, or every one with many=True.

        An empty query is refused rather than taken to mean everything.
        Returns how many documents were deleted.
        """
        if not query:
            raise ValueError("Refusing to delete with an empty query.")
        c = collection(collection_name)
        return c.delete_many(query) if many else c.delete_one(query)

    @server.tool(annotations=DESTROY)
    @_reported
    def execute(statement: str, params: Optional[list[Any]] = None) -> dict[str, Any]:
        """Run one PostgreSQL statement that may change the database."""
        return _capped(db.query(statement, params or None))

    return server


def main(argv: Optional[list[str]] = None) -> None:
    parser = argparse.ArgumentParser(
        prog="dbcenter-mcp",
        description="Serve a DBCenter database to MCP clients over stdio.",
    )
    parser.add_argument("--path", required=True, help="the database directory")
    parser.add_argument(
        "--allow-writes",
        action="store_true",
        help="add the insert, update, delete and execute tools (default: read-only)",
    )
    parser.add_argument(
        "--multi-user-sync",
        action="store_true",
        help="open the database in sync mode, as it was created",
    )
    parser.add_argument(
        "--postgresql", type=int, default=None, help="PostgreSQL major version"
    )
    args = parser.parse_args(argv)

    db = dbcenter.DBCenter(
        path=args.path,
        persist=True,
        multi_user_sync=args.multi_user_sync,
        postgresql=args.postgresql,
    )
    atexit.register(db.shutdown)
    build_server(db, allow_writes=args.allow_writes).run("stdio")


if __name__ == "__main__":
    main()
