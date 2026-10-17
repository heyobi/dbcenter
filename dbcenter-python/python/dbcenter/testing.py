"""pytest fixtures: a real PostgreSQL for your tests, no Docker, no server.

Installing dbcenter is enough - pytest finds these through an entry point:

    def test_orders(dbcenter_db):
        dbcenter_db.query("CREATE TABLE orders (id serial, total numeric)")
        dbcenter_db.query("INSERT INTO orders (total) VALUES (10)")
        assert dbcenter_db.query("SELECT count(*) AS n FROM orders")[0]["n"] == 1

    def test_with_sqlalchemy(postgres_url):
        engine = sqlalchemy.create_engine(postgres_url.replace("postgresql://", "postgresql+psycopg://"))

One server is started per test session and removed at the end. Each test gets
an empty `public` schema: whatever a test creates there is dropped after it,
so tests cannot see each other's tables.

The server cannot run as root (PostgreSQL refuses), which matters in some CI
containers; run the tests as an ordinary user.
"""

import pytest

import dbcenter


@pytest.fixture(scope="session")
def dbcenter_server(tmp_path_factory):
    """The session's DBCenter, shared by every test. Prefer `dbcenter_db`."""
    db = dbcenter.DBCenter(path=str(tmp_path_factory.mktemp("dbcenter")), persist=False)
    yield db
    db.shutdown()


def _reset(db):
    db.query("DROP SCHEMA public CASCADE")
    db.query("CREATE SCHEMA public")


@pytest.fixture
def dbcenter_db(dbcenter_server):
    """A DBCenter whose `public` schema is empty at the start of the test."""
    yield dbcenter_server
    _reset(dbcenter_server)


@pytest.fixture
def postgres_dsn(dbcenter_db):
    """A libpq connection string for the test's database, for psycopg or psql."""
    return dbcenter_db.dsn


@pytest.fixture
def postgres_url(dbcenter_db):
    """A postgresql:// URL for the test's database, for SQLAlchemy and ORMs."""
    return dbcenter_db.url
