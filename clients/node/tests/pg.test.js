// node-postgres: text protocol defaults, pg-types parsing, parameterised and named prepared
// statements, transactions, errors with SQLSTATE fields, COPY and LISTEN/NOTIFY.
import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { pipeline } from 'node:stream/promises';
import { Readable, Writable } from 'node:stream';
import pg from 'pg';
import copyStreams from 'pg-copy-streams';
import { connect, startDb } from './support.js';

let db;
let c;

before(async () => {
  db = await startDb();
  c = await connect(db);
  await c.query(`CREATE TABLE t (id serial PRIMARY KEY, name text NOT NULL UNIQUE, n int, note text)`);
});

after(async () => {
  await c?.end();
  await db?.stop();
});

describe('types (pg-types text parsing)', () => {
  it('parses scalars', async () => {
    const r = await c.query(`SELECT 1::int2 AS i2, 2::int4 AS i4, 9007199254740993::int8 AS i8,
      1.5::float8 AS f8, 12345.6789::numeric AS num, true AS b, 'héllo'::text AS s, NULL::int AS nul`);
    assert.deepEqual(r.rows[0], { i2: 1, i4: 2, i8: '9007199254740993', f8: 1.5, num: '12345.6789', b: true, s: 'héllo', nul: null });
    assert.deepEqual(r.fields.map((f) => [f.name, f.dataTypeID]), [
      ['i2', 21], ['i4', 23], ['i8', 20], ['f8', 701], ['num', 1700], ['b', 16], ['s', 25], ['nul', 23],
    ]);
  });

  it('parses dates and timestamps with the session time zone', async () => {
    await c.query(`SET TimeZone = 'UTC'`);
    const r = await c.query(`SELECT '2024-03-05 06:07:08.123'::timestamp AS ts, '2024-03-05 06:07:08.123+02'::timestamptz AS tstz,
      '2024-03-05'::date AS d, '04:05:06'::time AS tm, '1 day 02:00:00'::interval AS iv`);
    const row = r.rows[0];
    assert.ok(row.tstz instanceof Date);
    assert.equal(row.tstz.toISOString(), '2024-03-05T04:07:08.123Z');
    assert.ok(row.ts instanceof Date);
    assert.equal(row.ts.getFullYear(), 2024);
    assert.equal(row.ts.getMonth(), 2);
    assert.equal(row.ts.getHours(), 6);
    assert.equal(row.ts.getMilliseconds(), 123);
    assert.equal(row.d.getFullYear(), 2024);
    assert.equal(row.d.getDate(), 5);
    assert.equal(row.tm, '04:05:06');
    assert.equal(row.iv.days, 1);
    assert.equal(row.iv.hours, 2);
    await c.query(`SET TimeZone = 'America/New_York'`);
    const t = await c.query(`SELECT '2024-07-01 12:00:00+00'::timestamptz::text AS s`);
    assert.equal(t.rows[0].s, '2024-07-01 08:00:00-04');
    await c.query(`RESET TimeZone`);
  });

  it('parses json, jsonb and arrays', async () => {
    const r = await c.query(`SELECT '{"a":[1,2,{"b":null}]}'::json AS j, '{"a": 1}'::jsonb AS jb,
      ARRAY[1,2,3] AS ia, ARRAY['a','b,c',NULL,'d"e'] AS ta, ARRAY[[1,2],[3,4]] AS m, '{}'::int[] AS e,
      ARRAY[1.5,2.5]::float8[] AS fa, ARRAY[true,false] AS ba, ARRAY[9007199254740993]::int8[] AS i8a`);
    const row = r.rows[0];
    assert.deepEqual(row.j, { a: [1, 2, { b: null }] });
    assert.deepEqual(row.jb, { a: 1 });
    assert.deepEqual(row.ia, [1, 2, 3]);
    assert.deepEqual(row.ta, ['a', 'b,c', null, 'd"e']);
    assert.deepEqual(row.m, [[1, 2], [3, 4]]);
    assert.deepEqual(row.e, []);
    assert.deepEqual(row.fa, [1.5, 2.5]);
    assert.deepEqual(row.ba, [true, false]);
    assert.deepEqual(row.i8a, ['9007199254740993']);
  });

  it('parses bytea and uuid', async () => {
    const r = await c.query(`SELECT '\\x00ff10'::bytea AS b, '550e8400-e29b-41d4-a716-446655440000'::uuid AS u`);
    assert.ok(Buffer.isBuffer(r.rows[0].b));
    assert.deepEqual([...r.rows[0].b], [0, 255, 16]);
    assert.equal(r.rows[0].u, '550e8400-e29b-41d4-a716-446655440000');
  });

  it('round-trips bound values of each type', async () => {
    const when = new Date('2024-03-05T06:07:08.123Z');
    const r = await c.query(
      `SELECT $1::int AS i, $2::text AS s, $3::bytea AS b, $4::jsonb AS j, $5::int[] AS a, $6::timestamptz AS t, $7::boolean AS bo, $8::text AS nul`,
      [42, 'x', Buffer.from([1, 2, 3]), { k: [1, 2] }, [1, 2, 3], when, false, null],
    );
    const row = r.rows[0];
    assert.equal(row.i, 42);
    assert.equal(row.s, 'x');
    assert.deepEqual([...row.b], [1, 2, 3]);
    assert.deepEqual(row.j, { k: [1, 2] });
    assert.deepEqual(row.a, [1, 2, 3]);
    assert.equal(row.t.toISOString(), when.toISOString());
    assert.equal(row.bo, false);
    assert.equal(row.nul, null);
  });

  it('honours custom type parsers per query', async () => {
    const r = await c.query({ text: 'SELECT 7::int8 AS n', types: { getTypeParser: (oid, fmt) => (oid === 20 ? (v) => BigInt(v) : pg.types.getTypeParser(oid, fmt)) } });
    assert.equal(r.rows[0].n, 7n);
    const rows = await c.query({ text: 'SELECT 1 AS a, 2 AS b', rowMode: 'array' });
    assert.deepEqual(rows.rows, [[1, 2]]);
  });
});

describe('queries', () => {
  it('runs parameterised DML with command tags', async () => {
    const ins = await c.query('INSERT INTO t (name, n) VALUES ($1, $2), ($3, $4) RETURNING id, name', ['a', 1, 'b', 2]);
    assert.equal(ins.command, 'INSERT');
    assert.equal(ins.rowCount, 2);
    assert.deepEqual(ins.rows.map((r) => r.name), ['a', 'b']);
    const upd = await c.query('UPDATE t SET n = n + $1 WHERE name = $2', [10, 'a']);
    assert.equal(upd.command, 'UPDATE');
    assert.equal(upd.rowCount, 1);
    const sel = await c.query('SELECT n FROM t WHERE name = ANY($1) ORDER BY name', [['a', 'b']]);
    assert.deepEqual(sel.rows.map((r) => r.n), [11, 2]);
    const del = await c.query('DELETE FROM t');
    assert.equal(del.rowCount, 2);
  });

  it('returns one result per statement in the simple protocol', async () => {
    const rs = await c.query('SELECT 1 AS a; SELECT 2 AS b, 3 AS c');
    assert.equal(rs.length, 2);
    assert.deepEqual(rs[0].rows, [{ a: 1 }]);
    assert.deepEqual(rs[1].rows, [{ b: 2, c: 3 }]);
  });

  it('reuses named prepared statements', async () => {
    const q = { name: 'by-name', text: 'SELECT $1::int + 1 AS n' };
    for (let i = 0; i < 4; i++) {
      const r = await c.query({ ...q, values: [i] });
      assert.equal(r.rows[0].n, i + 1);
    }
    const r = await c.query(`SELECT name FROM pg_prepared_statements WHERE name = 'by-name'`);
    assert.equal(r.rowCount, 1);
  });
});

describe('transactions and errors', () => {
  it('commits and rolls back', async () => {
    await c.query('BEGIN');
    await c.query(`INSERT INTO t (name) VALUES ('tx1')`);
    await c.query('ROLLBACK');
    assert.equal((await c.query(`SELECT 1 FROM t WHERE name = 'tx1'`)).rowCount, 0);
    await c.query('BEGIN');
    await c.query(`INSERT INTO t (name) VALUES ('tx2')`);
    await c.query('SAVEPOINT s');
    await c.query(`INSERT INTO t (name) VALUES ('tx3')`);
    await c.query('ROLLBACK TO s');
    await c.query('COMMIT');
    const r = await c.query(`SELECT name FROM t WHERE name LIKE 'tx%'`);
    assert.deepEqual(r.rows.map((x) => x.name), ['tx2']);
    await c.query(`DELETE FROM t`);
  });

  it('reports SQLSTATE and constraint fields on errors', async () => {
    await c.query(`INSERT INTO t (name) VALUES ('dup')`);
    await assert.rejects(c.query(`INSERT INTO t (name) VALUES ('dup')`), (e) => {
      assert.ok(e instanceof pg.DatabaseError);
      assert.equal(e.code, '23505');
      assert.equal(e.severity, 'ERROR');
      assert.equal(e.constraint, 't_name_key');
      assert.equal(e.table, 't');
      assert.equal(e.schema, 'public');
      assert.equal(e.detail, 'Key (name)=(dup) already exists.');
      assert.equal(e.message, 'duplicate key value violates unique constraint "t_name_key"');
      return true;
    });
    await assert.rejects(c.query(`INSERT INTO t (name) VALUES (NULL)`), (e) => {
      assert.equal(e.code, '23502');
      assert.equal(e.column, 'name');
      return true;
    });
    await assert.rejects(c.query('SELECT 1/0'), (e) => e.code === '22012' && e.message === 'division by zero');
    await assert.rejects(c.query('SELECT * FROM nope'), (e) => e.code === '42P01' && e.message === 'relation "nope" does not exist');
    await assert.rejects(c.query('SELEC 1'), (e) => e.code === '42601' && /syntax error/.test(e.message)); // pgmem sends no position (P) field yet
    await assert.rejects(c.query('SELECT $1::int', ['abc']), (e) => e.code === '22P02');
    await c.query(`DELETE FROM t`);
  });

  it('aborts a transaction after an error until rollback', async () => {
    await c.query('BEGIN');
    await assert.rejects(c.query('SELECT 1/0'));
    await assert.rejects(c.query('SELECT 1'), (e) => e.code === '25P02');
    await c.query('ROLLBACK');
    assert.equal((await c.query('SELECT 1 AS n')).rows[0].n, 1);
  });

  it('keeps the connection usable after an error in an extended query', async () => {
    await assert.rejects(c.query({ name: 'bad', text: 'SELECT $1::int / 0', values: [1] }), (e) => e.code === '22012');
    assert.equal((await c.query('SELECT 2 AS n')).rows[0].n, 2);
  });

  it('isolates concurrent connections', async () => {
    const other = await connect(db);
    try {
      await c.query('BEGIN');
      await c.query(`INSERT INTO t (name) VALUES ('iso')`);
      assert.equal((await other.query(`SELECT 1 FROM t WHERE name = 'iso'`)).rowCount, 0);
      await c.query('COMMIT');
      assert.equal((await other.query(`SELECT 1 FROM t WHERE name = 'iso'`)).rowCount, 1);
      await c.query('DELETE FROM t');
    } finally {
      await other.end();
    }
  });
});

describe('pool', () => {
  it('serves queries and transactions from a pool', async () => {
    const pool = new pg.Pool({ connectionString: db.url, max: 3 });
    try {
      const rs = await Promise.all([1, 2, 3, 4, 5].map((i) => pool.query('SELECT $1::int AS n', [i])));
      assert.deepEqual(rs.map((r) => r.rows[0].n), [1, 2, 3, 4, 5]);
      const cl = await pool.connect();
      try {
        await cl.query('BEGIN');
        await cl.query(`INSERT INTO t (name) VALUES ('pooled')`);
        await cl.query('COMMIT');
      } finally {
        cl.release();
      }
      assert.equal((await pool.query(`SELECT count(*)::int AS n FROM t WHERE name = 'pooled'`)).rows[0].n, 1);
      await pool.query(`DELETE FROM t`);
    } finally {
      await pool.end();
    }
  });
});

describe('COPY (pg-copy-streams)', () => {
  it('copies in and out', async () => {
    const input = Readable.from([Buffer.from('c1\t1\nc2\t\\N\n'), Buffer.from('c3\t3\n')]);
    await pipeline(input, c.query(copyStreams.from('COPY t (name, n) FROM STDIN')));
    const chunks = [];
    await pipeline(
      c.query(copyStreams.to('COPY (SELECT name, n FROM t ORDER BY name) TO STDOUT')),
      new Writable({ write(chunk, _e, cb) { chunks.push(chunk); cb(); } }),
    );
    assert.equal(Buffer.concat(chunks).toString(), 'c1\t1\nc2\t\\N\nc3\t3\n');
    await c.query('DELETE FROM t');
  });
});

describe('LISTEN / NOTIFY', () => {
  it('delivers notifications to a listener', async () => {
    const listener = await connect(db);
    try {
      const got = new Promise((res) => listener.once('notification', res));
      await listener.query('LISTEN chan');
      await c.query(`SELECT pg_notify('chan', 'hello')`);
      const msg = await Promise.race([got, new Promise((_, rej) => setTimeout(() => rej(new Error('no notification')), 5000))]);
      assert.equal(msg.channel, 'chan');
      assert.equal(msg.payload, 'hello');
      assert.equal(typeof msg.processId, 'number');
      const got2 = new Promise((res) => listener.once('notification', res));
      await c.query(`NOTIFY chan, 'second'`);
      assert.equal((await got2).payload, 'second');
    } finally {
      await listener.end();
    }
  });

  it('delivers a notification only after commit', async () => {
    const listener = await connect(db);
    try {
      const seen = [];
      listener.on('notification', (m) => seen.push(m.payload));
      await listener.query('LISTEN txchan');
      await c.query('BEGIN');
      await c.query(`NOTIFY txchan, 'in-tx'`);
      await listener.query('SELECT 1');
      assert.deepEqual(seen, []);
      await c.query('COMMIT');
      await listener.query('SELECT 1');
      await new Promise((r) => setTimeout(r, 100));
      assert.deepEqual(seen, ['in-tx']);
    } finally {
      await listener.end();
    }
  });
});
