"""psycopg 3: text and binary loaders/dumpers, server-side binding, executemany, pipeline mode,
COPY, transactions and savepoints, error classes and diagnostics, prepared statements and
server-side cursors."""

import datetime as dt
import decimal
import ipaddress
import uuid

import psycopg
import pytest
from psycopg import errors, sql
from psycopg.rows import dict_row, namedtuple_row
from psycopg.types.json import Jsonb

UTC = dt.timezone.utc


@pytest.fixture
def t(conn):
    conn.execute("CREATE TABLE t (id serial PRIMARY KEY, name text NOT NULL UNIQUE, n int)")
    conn.commit()
    return conn


@pytest.mark.parametrize("binary", [False, True])
def test_loaders(conn, binary):
    cur = conn.cursor(binary=binary)
    cur.execute(
        """SELECT 1::int2, 2::int4, 9007199254740993::int8, 1.5::float8, 1.25::float4,
        12345.6789::numeric, true, 'héllo'::text, 'ab'::varchar(5), NULL::int,
        '\\x00ff10'::bytea, '550e8400-e29b-41d4-a716-446655440000'::uuid,
        '2024-03-05'::date, '04:05:06.5'::time, '2024-03-05 06:07:08.123456'::timestamp,
        '2024-03-05 06:07:08+02'::timestamptz, '1 day 02:03:04'::interval"""
    )
    row = cur.fetchone()
    assert row == (
        1, 2, 9007199254740993, 1.5, 1.25, decimal.Decimal("12345.6789"), True, "héllo", "ab", None,
        b"\x00\xff\x10", uuid.UUID("550e8400-e29b-41d4-a716-446655440000"),
        dt.date(2024, 3, 5), dt.time(4, 5, 6, 500000), dt.datetime(2024, 3, 5, 6, 7, 8, 123456),
        dt.datetime(2024, 3, 5, 4, 7, 8, tzinfo=UTC), dt.timedelta(days=1, hours=2, minutes=3, seconds=4),
    )  # fmt: skip
    assert row[-2].utcoffset() == dt.timedelta(0)


@pytest.mark.parametrize("binary", [False, True])
def test_json_arrays_and_nulls(conn, binary):
    cur = conn.cursor(binary=binary)
    cur.execute(
        """SELECT '{"a":[1,2,{"b":null}]}'::json, '{"a": 1}'::jsonb, ARRAY[1,2,3], ARRAY['a','b,c',NULL,'d"e'],
        ARRAY[[1,2],[3,4]], '{}'::int[], ARRAY[1.5,2.5]::float8[], ARRAY[true,NULL]::bool[],
        ARRAY[1.5,2]::numeric[], ARRAY['2024-01-01']::date[]"""
    )
    assert cur.fetchone() == (
        {"a": [1, 2, {"b": None}]}, {"a": 1}, [1, 2, 3], ["a", "b,c", None, 'd"e'],
        [[1, 2], [3, 4]], [], [1.5, 2.5], [True, None],
        [decimal.Decimal("1.5"), decimal.Decimal("2")], [dt.date(2024, 1, 1)],
    )  # fmt: skip


def test_text_numeric_special_and_inet(conn):
    cur = conn.cursor()
    cur.execute("SELECT 'NaN'::numeric, 'Infinity'::float8, '0.000'::numeric, '10.0.0.1/24'::inet")
    nan, inf, zero, inet = cur.fetchone()
    assert nan.is_nan()
    assert inf == float("inf")
    assert str(zero) == "0.000"
    assert inet == ipaddress.ip_interface("10.0.0.1/24")


@pytest.mark.parametrize("binary", [False, True])
def test_dumpers_round_trip(conn, binary):
    values = [
        42, -7, 2**40, 1.5, decimal.Decimal("123.4500"), "text ' \" \\ é", b"\x00\x01\xff", True, None,
        dt.date(2024, 3, 5), dt.time(1, 2, 3), dt.datetime(2024, 3, 5, 6, 7, 8, 9),
        dt.datetime(2024, 3, 5, 6, 7, 8, tzinfo=dt.timezone(dt.timedelta(hours=-5))),
        dt.timedelta(days=3, seconds=4, microseconds=5), uuid.UUID(int=12345),
        Jsonb({"k": [1, None, "x"]}), [1, 2, 3], [[1], [2]],
    ]  # fmt: skip
    for v in values:
        cur = conn.cursor(binary=binary)
        got = cur.execute("SELECT %b" if binary else "SELECT %s", (v,)).fetchone()[0]
        assert got == (v.obj if isinstance(v, Jsonb) else v), v


def test_text_array_dump(conn):
    assert conn.execute("SELECT %s::text[]", (["a", None, "b,c"],)).fetchone() == (["a", None, "b,c"],)


def test_auto_format_and_server_side_binding(conn):
    # psycopg 3 sends parameters separately (extended protocol); the server types them.
    cur = conn.cursor()
    assert cur.execute("SELECT %s::int + %s::int", (1, 2)).fetchone() == (3,)
    assert cur.execute("SELECT pg_typeof(%s::int8)", (1,)).fetchone() == ("bigint",)
    assert cur.execute("SELECT %(a)s::text || %(b)s::text", {"a": "x", "b": "y"}).fetchone() == ("xy",)
    assert cur.execute("SELECT %s::text", ("it's",)).fetchone() == ("it's",)
    assert cur.execute("SELECT 'a%%b' || %s::text", ("c",)).fetchone() == ("a%bc",)
    assert cur.execute("SELECT %s = ANY(%s)", (2, [1, 2, 3])).fetchone() == (True,)
    assert cur.execute("SELECT %s::int IS NULL", (None,)).fetchone() == (True,)
    with pytest.raises(errors.SyntaxError):
        cur.execute("SELECT 1 +", prepare=False)
    conn.rollback()
    q = sql.SQL("SELECT {} FROM (VALUES (1)) AS {}(c)").format(sql.Identifier("c"), sql.Identifier("v"))
    assert cur.execute(q).fetchone() == (1,)
    assert cur.execute("SELECT 1").description[0].name == "?column?"
    assert cur.description[0].type_code == 23


def test_row_factories(t):
    t.execute("INSERT INTO t (name, n) VALUES ('a', 1)")
    assert t.cursor(row_factory=dict_row).execute("SELECT name, n FROM t").fetchone() == {"name": "a", "n": 1}
    r = t.cursor(row_factory=namedtuple_row).execute("SELECT name, n FROM t").fetchone()
    assert (r.name, r.n) == ("a", 1)


def test_executemany_and_returning(t):
    cur = t.cursor()
    cur.executemany("INSERT INTO t (name, n) VALUES (%s, %s)", [("a", 1), ("b", 2), ("c", None)])
    assert cur.rowcount == 3
    cur.executemany("INSERT INTO t (name, n) VALUES (%s, %s) RETURNING id", [("d", 4), ("e", 5)], returning=True)
    ids = []
    while True:
        ids.append(cur.fetchone()[0])
        if not cur.nextset():
            break
    assert len(ids) == 2
    assert t.execute("SELECT count(*), sum(n) FROM t").fetchone() == (5, 12)
    upd = t.execute("UPDATE t SET n = n * 2 WHERE n IS NOT NULL")
    assert upd.rowcount == 4 and upd.statusmessage == "UPDATE 4"


def test_pipeline_mode(t):
    with t.pipeline():
        c1 = t.execute("INSERT INTO t (name, n) VALUES (%s, %s) RETURNING id", ("p1", 1))
        c2 = t.execute("INSERT INTO t (name, n) VALUES (%s, %s) RETURNING id", ("p2", 2))
    assert c1.fetchone()[0] < c2.fetchone()[0]
    assert t.execute("SELECT count(*) FROM t").fetchone() == (2,)
    t.commit()
    # an error inside a pipeline aborts the rest up to the sync point
    with pytest.raises(errors.UniqueViolation):
        with t.pipeline():
            t.execute("INSERT INTO t (name) VALUES ('p1')")
            t.execute("SELECT 1")
    t.rollback()
    assert t.execute("SELECT count(*) FROM t").fetchone() == (2,)


def test_copy_from_and_to(t):
    with t.cursor().copy("COPY t (name, n) FROM STDIN") as copy:
        copy.write_row(("c1", 1))
        copy.write_row(("c2", None))
        copy.write(b"c3\t3\n")
    with t.cursor().copy("COPY t (name, n) FROM STDIN (FORMAT csv)") as copy:
        copy.write(b'"c,4",4\n')
    rows = []
    with t.cursor().copy("COPY (SELECT name, n FROM t ORDER BY n NULLS LAST) TO STDOUT") as copy:
        rows = list(copy.rows())
    assert rows == [("c1", "1"), ("c3", "3"), ("c,4", "4"), ("c2", None)]
    with t.cursor().copy("COPY (SELECT name, n FROM t ORDER BY n NULLS LAST) TO STDOUT") as copy:
        assert b"".join(bytes(d) for d in copy) == b"c1\t1\nc3\t3\nc,4\t4\nc2\t\\N\n"
    with t.cursor().copy("COPY (SELECT name, n FROM t ORDER BY n NULLS LAST) TO STDOUT (FORMAT csv)") as copy:
        assert b"".join(bytes(d) for d in copy) == b'c1,1\nc3,3\n"c,4",4\nc2,\n'
    # typed rows with set_types
    with t.cursor().copy("COPY (SELECT name, n FROM t ORDER BY n NULLS LAST) TO STDOUT") as copy:
        copy.set_types(["text", "int4"])
        assert list(copy.rows())[1] == ("c3", 3)
    t.commit()


def test_copy_binary_and_error(t):
    with t.cursor().copy("COPY t (name, n) FROM STDIN (FORMAT binary)") as copy:
        copy.set_types(["text", "int4"])
        copy.write_row(("b1", 10))
        copy.write_row(("b2", None))
    with t.cursor().copy("COPY (SELECT name, n FROM t ORDER BY n NULLS LAST) TO STDOUT (FORMAT binary)") as copy:
        copy.set_types(["text", "int4"])
        assert list(copy.rows()) == [("b1", 10), ("b2", None)]
    t.commit()
    with pytest.raises(errors.NotNullViolation):
        with t.cursor().copy("COPY t (name) FROM STDIN") as copy:
            copy.write(b"\\N\n")
    t.rollback()
    with pytest.raises(errors.BadCopyFileFormat):
        with t.cursor().copy("COPY t (name, n) FROM STDIN") as copy:
            copy.write(b"only-one-column\n")
    t.rollback()


def test_transactions_and_savepoints(dsn):
    with psycopg.connect(dsn) as a, psycopg.connect(dsn) as b:
        a.execute("CREATE TABLE t (name text PRIMARY KEY)")
        a.commit()
        a.execute("INSERT INTO t VALUES ('x')")
        assert a.info.transaction_status == psycopg.pq.TransactionStatus.INTRANS
        assert b.execute("SELECT count(*) FROM t").fetchone() == (0,)
        a.commit()
        assert b.execute("SELECT count(*) FROM t").fetchone() == (1,)
        b.commit()
        with a.transaction():
            a.execute("INSERT INTO t VALUES ('y')")
            with pytest.raises(errors.UniqueViolation):
                with a.transaction():  # nested: a savepoint
                    a.execute("INSERT INTO t VALUES ('z')")
                    a.execute("INSERT INTO t VALUES ('x')")
            a.execute("INSERT INTO t VALUES ('w')")
        assert sorted(r[0] for r in a.execute("SELECT name FROM t")) == ["w", "x", "y"]
        with pytest.raises(ZeroDivisionError):
            with a.transaction():
                a.execute("DELETE FROM t")
                raise ZeroDivisionError
        assert a.execute("SELECT count(*) FROM t").fetchone() == (3,)
        a.rollback()
        a.autocommit = True
        a.execute("INSERT INTO t VALUES ('auto')")
        assert b.execute("SELECT count(*) FROM t").fetchone() == (4,)
        b.commit()


def test_errors_and_diagnostics(t):
    t.execute("INSERT INTO t (name) VALUES ('dup')")
    with pytest.raises(errors.UniqueViolation) as ei:
        t.execute("INSERT INTO t (name) VALUES ('dup')")
    e = ei.value
    assert e.sqlstate == "23505"
    assert isinstance(e, errors.IntegrityError) and isinstance(e, psycopg.DatabaseError)
    d = e.diag
    assert d.severity_nonlocalized == "ERROR"
    assert d.constraint_name == "t_name_key"
    assert (d.table_name, d.schema_name) == ("t", "public")
    assert d.message_primary == 'duplicate key value violates unique constraint "t_name_key"'
    assert d.message_detail == "Key (name)=(dup) already exists."
    assert t.info.transaction_status == psycopg.pq.TransactionStatus.INERROR
    with pytest.raises(errors.InFailedSqlTransaction):
        t.execute("SELECT 1")
    t.rollback()
    with pytest.raises(errors.NotNullViolation) as ei:
        t.execute("INSERT INTO t (name) VALUES (NULL)")
    assert ei.value.diag.column_name == "name"
    t.rollback()
    for stmt, cls in [
        ("SELECT 1/0", errors.DivisionByZero),
        ("SELECT * FROM nope", errors.UndefinedTable),
        ("SELECT nope FROM t", errors.UndefinedColumn),
        ("SELECT 'x'::int", errors.InvalidTextRepresentation),
        ("SELEC 1", errors.SyntaxError),
        ("SELECT nofunc()", errors.UndefinedFunction),
    ]:
        with pytest.raises(cls):
            t.execute(stmt)
        t.rollback()
    with pytest.raises(errors.InvalidTextRepresentation):
        t.execute("SELECT %s::int", ("abc",))
    t.rollback()
    with pytest.raises(psycopg.ProgrammingError):
        t.execute("SELECT %s", ())  # missing parameter
    with pytest.raises(errors.UndefinedTable) as ei:
        t.execute("SELECT * FROM nope")
    assert ei.value.diag.message_primary == 'relation "nope" does not exist'


def test_prepared_statements(dsn):
    with psycopg.connect(dsn, prepare_threshold=2) as c:
        for i in range(5):
            assert c.execute("SELECT %s::int + 1", (i,)).fetchone() == (i + 1,)
        rows = c.execute("SELECT statement FROM pg_prepared_statements").fetchall()
        assert len(rows) == 1 and "$1" in rows[0][0]
        # prepare=True forces it, prepare=False never
        c.execute("SELECT 'forced'", prepare=True)
        c.execute("SELECT 'never'", prepare=False)
        names = [r[0] for r in c.execute("SELECT statement FROM pg_prepared_statements")]
        assert any("'forced'" in n for n in names)
        assert not any("'never'" in n for n in names)
        c.rollback()
        c.execute("DEALLOCATE ALL")
        assert c.execute("SELECT 5::int + %s::int", (1,)).fetchone() == (6,)
    with psycopg.connect(dsn, prepare_threshold=None) as c:
        for i in range(4):
            c.execute("SELECT %s::int", (i,))
        assert c.execute("SELECT count(*) FROM pg_prepared_statements").fetchone() == (0,)


def test_server_side_cursor(t):
    t.cursor().executemany("INSERT INTO t (name, n) VALUES (%s, %s)", [(f"r{i}", i) for i in range(10)])
    t.commit()
    with t.cursor(name="c1") as cur:
        cur.itersize = 3
        cur.execute("SELECT n FROM t ORDER BY n")
        assert cur.fetchone() == (0,)
        assert cur.fetchmany(3) == [(1,), (2,), (3,)]
        assert [r[0] for r in cur] == [4, 5, 6, 7, 8, 9]
        assert cur.fetchone() is None
    with t.cursor(name="c2", scrollable=True) as cur:
        cur.execute("SELECT n FROM t ORDER BY n")
        cur.scroll(5, "absolute")
        assert cur.fetchone() == (5,)
        cur.scroll(-2)
        assert cur.fetchone() == (4,)
    with t.cursor(name="c3", withhold=True) as cur:
        cur.execute("SELECT n FROM t ORDER BY n")
        t.commit()
        assert cur.fetchone() == (0,)


def test_notifies(dsn):
    with psycopg.connect(dsn, autocommit=True) as listener, psycopg.connect(dsn, autocommit=True) as sender:
        listener.execute("LISTEN chan")
        sender.execute("SELECT pg_notify('chan', 'hello')")
        got = list(listener.notifies(timeout=5, stop_after=1))
        assert [(n.channel, n.payload) for n in got] == [("chan", "hello")]
        seen = []
        listener.add_notify_handler(seen.append)
        sender.execute("NOTIFY chan, 'again'")
        listener.execute("SELECT 1")
        assert [(n.channel, n.payload) for n in seen] == [("chan", "again")]
