# DBCenter over MCP

`dbcenter-mcp` serves a DBCenter database to MCP clients - desktop assistants,
coding agents, anything that speaks the Model Context Protocol - over stdio.

```bash
pip install "dbcenter[mcp]"        # the mcp extra needs Python 3.10 or newer
dbcenter-mcp --path ./data         # read-only
```

The server owns the database: it starts PostgreSQL on that directory when the
client launches it and stops it when the client goes away. Nothing else can have
the same directory open meanwhile.

## Configuring a client

Most clients take a command and its arguments:

```json
{
  "mcpServers": {
    "dbcenter": {
      "command": "dbcenter-mcp",
      "args": ["--path", "/absolute/path/to/data"]
    }
  }
}
```

With Claude Code:

```bash
claude mcp add dbcenter -- dbcenter-mcp --path /absolute/path/to/data
```

Use an absolute path: a client starts the server from a working directory of
its own choosing. Add `--multi-user-sync` for a database created in sync mode,
and `--allow-writes` to add the tools that change data.

## Tools

Always:

| tool | what it does |
|---|---|
| `list_collections` | the collections in the database |
| `describe_collection` | a collection's fields, their PostgreSQL types, and its row count |
| `find` | documents matching a MongoDB-style query, at most `limit` (default 50) |
| `count` | how many documents match a query |
| `sql` | one PostgreSQL statement, read-only, with `$1, $2` parameters |

With `--allow-writes`:

| tool | what it does |
|---|---|
| `insert` | insert documents; a new collection is created on first insert |
| `update` | update the first match, or all with `many=true`; `$set` or plain values |
| `delete` | delete the first match, or all with `many=true`; an empty query is refused |
| `execute` | one PostgreSQL statement that may change the database |

Answers are capped at 200 rows and say when they were cut short.

## What read-only means here

The `sql` tool does not inspect the SQL. Each statement runs in a PostgreSQL
`READ ONLY` transaction that is rolled back afterwards, so the server itself
refuses anything that would change the database - an `INSERT`, a `DELETE` inside
a CTE, `nextval()`, any DDL - however it is phrased. `db.query(sql, params,
read_only=True)` is the same guarantee from Python. `python_tests/test_mcp.py`
tries each of those and checks that every one is refused and nothing changed.

The write tools are not registered at all without `--allow-writes`, so a client
cannot discover them, and every tool is annotated so a client that shows
confirmations knows which ones are read-only and which are destructive.

When a tool fails, the error says why - "No collection named 'orderz'",
"cannot execute INSERT in a read-only transaction" - so a model can correct its
next call rather than guess.

## Limits

- `find`'s `limit` is applied after the documents are fetched, because
  `Collection.find` has no limit of its own yet. On a large collection, use
  `sql` with `LIMIT`.
- One client at a time: the server is a stdio process started by the client.
  For several clients, run `dbcenter-serve` and connect them with a PostgreSQL
  driver instead.
