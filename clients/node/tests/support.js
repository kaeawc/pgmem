// Test database for one test file: a pgmem server of its own (the `pgmem` binary, PGMEM_BIN or
// ../../target/release/pgmem), or, with PGMEM_CLIENT_DSN set to a PostgreSQL URL
// (postgres://postgres@127.0.0.1:5432/postgres), a new database on that server.
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import pg from 'pg';

let counter = 0;

export const isPostgres = Boolean(process.env.PGMEM_CLIENT_DSN);

function binary() {
  if (process.env.PGMEM_BIN) return process.env.PGMEM_BIN;
  let dir = dirname(fileURLToPath(import.meta.url));
  for (;;) {
    const candidate = join(dir, 'target/release/pgmem');
    if (existsSync(candidate)) return candidate;
    const parent = resolve(dir, '..');
    if (parent === dir) return 'pgmem';
    dir = parent;
  }
}

export async function startDb() {
  const admin = process.env.PGMEM_CLIENT_DSN;
  if (admin) {
    const name = `pgmem_client_${process.pid}_${counter++}`;
    const c = new pg.Client({ connectionString: admin });
    await c.connect();
    await c.query(`CREATE DATABASE ${name}`);
    await c.end();
    const url = admin.slice(0, admin.lastIndexOf('/')) + '/' + name;
    return {
      url,
      stop: async () => {
        const a = new pg.Client({ connectionString: admin });
        await a.connect();
        await a.query(`DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
        await a.end();
      },
    };
  }
  const child = spawn(binary(), ['--port', '0'], { stdio: ['ignore', 'pipe', 'inherit'] });
  const rl = createInterface({ input: child.stdout });
  const url = await new Promise((res, rej) => {
    rl.once('line', (l) => res(l.trim()));
    child.once('error', rej);
    child.once('exit', () => rej(new Error('pgmem exited early')));
  });
  return {
    url,
    stop: async () => {
      child.removeAllListeners('exit');
      child.kill();
    },
  };
}

export async function connect(db, config = {}) {
  const client = new pg.Client({ connectionString: db.url, ...config });
  await client.connect();
  return client;
}
