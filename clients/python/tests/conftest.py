"""Test database: a pgmem server of its own per test (the `pgmem` binary: PGMEM_BIN or
target/release/pgmem), or, with PGMEM_CLIENT_DSN set to a PostgreSQL URL
(postgres://postgres@127.0.0.1:5432/postgres), a new database on that server."""

import itertools
import os
import subprocess
from pathlib import Path

import psycopg
import pytest

_counter = itertools.count()
IS_POSTGRES = bool(os.environ.get("PGMEM_CLIENT_DSN"))


def _binary() -> str:
    if os.environ.get("PGMEM_BIN"):
        return os.environ["PGMEM_BIN"]
    for d in [Path(__file__).resolve(), *Path(__file__).resolve().parents]:
        candidate = d / "target/release/pgmem"
        if candidate.exists():
            return str(candidate)
    return "pgmem"


@pytest.fixture
def dsn():
    admin = os.environ.get("PGMEM_CLIENT_DSN")
    if admin:
        name = f"pgmem_client_{os.getpid()}_{next(_counter)}"
        with psycopg.connect(admin, autocommit=True) as c:
            c.execute(f"CREATE DATABASE {name}")
        try:
            yield admin.rsplit("/", 1)[0] + "/" + name
        finally:
            with psycopg.connect(admin, autocommit=True) as c:
                c.execute(f"DROP DATABASE IF EXISTS {name} WITH (FORCE)")
        return
    child = subprocess.Popen([_binary(), "--port", "0"], stdout=subprocess.PIPE, text=True)
    try:
        yield child.stdout.readline().strip()
    finally:
        child.kill()
        child.wait()


@pytest.fixture
def conn(dsn):
    with psycopg.connect(dsn) as c:
        yield c
