'use strict';
// Knex query builder + schema builder + migrations over mysql2 and pg.
const path = require('path');
const { makeSuite, mkdb, HOSTILE, VIET, BLOB, assert, eq } = require(path.join(__dirname, '..', 'lib', 'harness.js'));
const Knex = require('knex');

async function app(label, cfg, known) {
  const S = makeSuite(`node-knex:${label}`);
  const C = (n, f) => S.run(n, f, known[n] || known['*'], !known[n]);
  const mysql = cfg.client === 'mysql2';
  const knex = Knex(cfg);
  try {
    if (!await C('connect+select1', async () => { const r = await knex.raw('select 1 as one'); assert(r, 'no result'); })) return;

    const fkDdl = mysql
      ? ['CREATE TABLE posts (id INT AUTO_INCREMENT PRIMARY KEY, user_id INT NOT NULL, title VARCHAR(300) NOT NULL, body TEXT, views INT NOT NULL DEFAULT 0, FOREIGN KEY (user_id) REFERENCES users(id))',
         'CREATE INDEX posts_user_id_index ON posts(user_id)',
         'CREATE TABLE comments (id INT AUTO_INCREMENT PRIMARY KEY, post_id INT NOT NULL, user_id INT NOT NULL, body TEXT NOT NULL, FOREIGN KEY (post_id) REFERENCES posts(id), FOREIGN KEY (user_id) REFERENCES users(id))']
      : ['CREATE TABLE posts (id integer primary key, user_id integer NOT NULL REFERENCES users(id), title varchar(300) NOT NULL, body text, views integer NOT NULL DEFAULT 0)',
         'CREATE INDEX posts_user_id_index ON posts(user_id)',
         'CREATE TABLE comments (id integer primary key, post_id integer NOT NULL REFERENCES posts(id), user_id integer NOT NULL REFERENCES users(id), body text NOT NULL)'];
    const pk = (t) => mysql ? t.increments('id') : t.specificType('id', 'integer primary key');
    let ok = await C('schema.createTable x3 (FK, unique, default ts, binary)', async () => {
      await knex.schema.createTable('users', (t) => {
        pk(t); t.string('email', 255).notNullable().unique(); t.string('name', 200).notNullable();
        t.text('profile'); t.binary('avatar'); t.timestamp('created_at').defaultTo(knex.fn.now());
      });
      await knex.schema.createTable('posts', (t) => {
        pk(t); t.integer('user_id').notNullable().references('id').inTable('users'); t.string('title', 300).notNullable();
        t.text('body'); t.integer('views').notNullable().defaultTo(0); t.index('user_id');
      });
      await knex.schema.createTable('comments', (t) => {
        pk(t); t.integer('post_id').notNullable().references('id').inTable('posts');
        t.integer('user_id').notNullable().references('id').inTable('users'); t.text('body').notNullable();
      });
    });
    if (!ok) {
      // Knex emits `ALTER TABLE .. ADD CONSTRAINT .. FOREIGN KEY / UNIQUE` (unsupported). Recreate the tables with
      // inline DDL so the remaining cases still exercise the gateway; the failure above stays recorded.
      for (const t of ['comments', 'posts']) await knex.raw(`DROP TABLE IF EXISTS ${t}`).catch(() => {});
      await knex.raw('DROP TABLE IF EXISTS users').catch(() => {});
      await knex.raw(mysql
        ? 'CREATE TABLE users (id INT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(255) NOT NULL UNIQUE, name VARCHAR(200) NOT NULL, profile TEXT, avatar BLOB, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)'
        : 'CREATE TABLE users (id integer primary key, email varchar(255) NOT NULL UNIQUE, name varchar(200) NOT NULL, profile text, avatar bytea, created_at timestamp DEFAULT CURRENT_TIMESTAMP)');
      for (const q of fkDdl) await knex.raw(q);
      ok = true;
    }
    if (!ok) return;

    if (!mysql) await C('schema increments() -> SERIAL', async () => {
      await knex.schema.createTable('serial_probe', (t) => { t.increments('id'); t.string('v'); });
      await knex('serial_probe').insert([{ v: 'a' }, { v: 'b' }]);
      const r = await knex('serial_probe').select('id').orderBy('v');
      assert(r[0].id !== null && Number(r[1].id) === Number(r[0].id) + 1, 'SERIAL ids: ' + JSON.stringify(r));
    });
    await C('schema.hasTable / hasColumn', async () => {
      assert(await knex.schema.hasTable('users'), 'hasTable users');
      assert(await knex.schema.hasColumn('users', 'email'), 'hasColumn email');
      assert(!(await knex.schema.hasTable('nope_x')), 'hasTable nope_x');
    });

    const ids = {};
    await C('insert + auto-increment id (select back)', async () => {
      await knex('users').insert({ email: 'ann@example.com', name: 'Ann', profile: JSON.stringify({ k: [1, 2] }) });
      await knex('users').insert({ email: 'bob@example.com', name: 'Bob' });
      const r = await knex('users').select('id', 'email').orderBy('email');
      assert(r[0].id !== null && r[1].id !== null, 'ids are NULL: ' + JSON.stringify(r));
      assert(Number(r[1].id) === Number(r[0].id) + 1, 'ids not sequential');
      ids.a = Number(r[0].id); ids.b = Number(r[1].id);
    });
    await C('insert returning id / insertId', async () => {
      const r = mysql ? await knex('users').insert({ email: 'li@x', name: 'LI' })
                      : await knex('users').insert({ email: 'li@x', name: 'LI' }).returning('id');
      const id = Array.isArray(r) ? (typeof r[0] === 'object' && r[0] !== null ? r[0].id : r[0]) : r;
      try { assert(Number(id) === ids.b + 1, 'returned id ' + JSON.stringify(r)); }
      finally { await knex('users').where({ email: 'li@x' }).del(); }
    });
    await C('result types (int is number)', async () => {
      const r = await knex('users').count({ n: '*' }).first();
      const v = r.n !== undefined ? r.n : Object.values(r)[0];
      // node-postgres returns int8 (COUNT) as a numeric string on real PostgreSQL too; only that is accepted for pg.
      assert(typeof v === 'number' || (typeof v === 'string' && !mysql && /^\d+$/.test(v)), `count() returned ${typeof v}: ${JSON.stringify(v)}`);
    });
    await C('timestamp default', async () => {
      const r = await knex('users').where({ id: ids.a }).first();
      assert(r.created_at, 'created_at empty');
      const d = new Date(r.created_at); assert(!isNaN(d) && Math.abs(Date.now() - d.getTime()) < 36e5 * 26, 'bad ts ' + r.created_at);
    });
    await C('unique violation -> error code', async () => {
      try { await knex('users').insert({ email: 'ann@example.com', name: 'dup' }); } catch (e) {
        const code = e.code || e.errno;
        if (mysql) assert(e.errno === 1062 || e.code === 'ER_DUP_ENTRY', `code ${e.code}/${e.errno}`);
        else assert(e.code === '23505', `SQLSTATE ${e.code}`);
        return;
      }
      throw new Error('duplicate accepted');
    });
    await C('foreign key violation -> error code', async () => {
      try { await knex('posts').insert({ user_id: 99999, title: 'orphan' }); } catch (e) {
        if (mysql) assert([1452, 1216].includes(e.errno), `code ${e.code}/${e.errno}`);
        else assert(e.code === '23503', `SQLSTATE ${e.code}`);
        return;
      }
      throw new Error('FK violation accepted');
    });
    await C('transaction commit + rollback', async () => {
      await knex.transaction(async (trx) => { await trx('posts').insert({ user_id: ids.a, title: 'keep' }); });
      try {
        await knex.transaction(async (trx) => { await trx('posts').insert({ user_id: ids.a, title: 'gone' }); throw new Error('boom'); });
      } catch (e) { assert(e.message === 'boom', e.message); }
      const t = (await knex('posts').select('title')).map((r) => r.title);
      eq(t, ['keep']);
    });
    await C('nested transaction (savepoint)', async () => {
      await knex.transaction(async (trx) => {
        await trx('posts').insert({ user_id: ids.a, title: 'sp-outer' });
        try { await trx.transaction(async (t2) => { await t2('posts').insert({ user_id: ids.a, title: 'sp-inner' }); throw new Error('x'); }); } catch (e) {}
      });
      const t = (await knex('posts').select('title').whereIn('title', ['sp-outer', 'sp-inner'])).map((r) => r.title);
      eq(t, ['sp-outer']);
    });
    await C('bulk insert (500 rows) / batchInsert', async () => {
      const rows = Array.from({ length: 500 }, (_, i) => ({ user_id: ids.b, title: `bulk-${String(i).padStart(4, '0')}`, body: 'x'.repeat(i % 30), views: i }));
      await knex.batchInsert('posts', rows, 100);
      const r = await knex('posts').where('title', 'like', 'bulk-%').count({ n: '*' }).sum({ s: 'views' }).first();
      assert(Number(r.n) === 500 && Number(r.s) === 124750, JSON.stringify(r));
    });
    await C('pagination limit/offset', async () => {
      const r = (await knex('posts').select('title').where('title', 'like', 'bulk-%').orderBy('title').limit(10).offset(20)).map((x) => x.title);
      assert(r.length === 10 && r[0] === 'bulk-0020' && r[9] === 'bulk-0029', JSON.stringify(r));
    });
    await C('join + group by/having + aggregates', async () => {
      await knex('comments').insert(knex('posts').select('id', knex.raw('?', [ids.b]), knex.raw('?', ['hi'])).where('title', 'keep'), undefined)
        .catch(async () => { const p = await knex('posts').where('title', 'keep').first(); await knex('comments').insert({ post_id: p.id, user_id: ids.b, body: 'hi' }); });
      const rows = await knex('users as u').leftJoin('posts as p', 'p.user_id', 'u.id')
        .select('u.name').count({ n: 'p.id' }).sum({ v: 'p.views' }).groupBy('u.id', 'u.name').having(knex.raw('count(p.id) >= 1')).orderBy('u.id');
      eq(rows.map((r) => r.name), ['Ann', 'Bob']); assert(Number(rows[1].n) === 500, JSON.stringify(rows));
      const j = await knex('comments as c').join('posts as p', 'p.id', 'c.post_id').join('users as u', 'u.id', 'c.user_id').select('p.title', 'c.body', 'u.name');
      eq(j.map((r) => [r.title, r.body, r.name]), [['keep', 'hi', 'Bob']]);
    });
    await C('json text column + whereIn', async () => {
      const r = await knex('users').where({ id: ids.a }).first();
      eq(JSON.parse(r.profile), { k: [1, 2] });
      const r2 = await knex('users').whereIn('id', [ids.a, ids.b, 424242]).orderBy('id');
      assert(r2.length === 2, 'whereIn');
    });
    await C('hostile strings as bound params', async () => {
      for (let i = 0; i < HOSTILE.length; i++) await knex('users').insert({ email: `h${i}@x`, name: HOSTILE[i], profile: HOSTILE[i] });
      const got = (await knex('users').where('email', 'like', 'h%@x').orderBy('id')).map((r) => r.name);
      eq(got, HOSTILE);
      const c = await knex('users').where({ name: HOSTILE[0] }).count({ n: '*' }).first(); assert(Number(c.n) === 1, 'count');
      const t = await knex('users').count({ n: '*' }).first(); assert(Number(t.n) === 2 + HOSTILE.length, 'table intact');
    });
    await C('unicode Vietnamese + emoji', async () => {
      await knex('users').insert({ email: 'v@x', name: VIET, profile: JSON.stringify({ v: VIET }) });
      const r = await knex('users').where({ name: VIET }).first();
      assert(r && r.name === VIET && JSON.parse(r.profile).v === VIET, 'mismatch');
    });
    await C('BLOB roundtrip', async () => {
      await knex('users').where({ id: ids.b }).update({ avatar: BLOB });
      const r = await knex('users').where({ id: ids.b }).first();
      assert(Buffer.isBuffer(r.avatar) && Buffer.compare(r.avatar, BLOB) === 0, 'blob mismatch: ' + (r.avatar && r.avatar.length));
    });
    await C('update/delete affected rows', async () => {
      const n = await knex('posts').where('title', 'like', 'bulk-%').update({ views: knex.raw('views + 1') });
      assert(n === 500, 'update count ' + n);
      const d = await knex('posts').where('title', 'like', 'bulk-%').del();
      assert(d === 500, 'delete count ' + d);
    });
    await C('upsert (onConflict merge / ignore)', async () => {
      await knex('users').insert({ email: 'ann@example.com', name: 'Ann2' }).onConflict('email').merge();
      const r = await knex('users').where({ email: 'ann@example.com' }).first(); assert(r.name === 'Ann2', r.name);
      await knex('users').insert({ email: 'ann@example.com', name: 'Ann3' }).onConflict('email').ignore();
      assert((await knex('users').where({ email: 'ann@example.com' }).first()).name === 'Ann2', 'ignore changed row');
    });
    await C('migrations (migrate.latest / rollback)', async () => {
      const mig = { getMigrations: async () => ['001_a', '002_b'], getMigrationName: (m) => m,
        getMigration: (m) => m === '001_a'
          ? { up: (k) => k.schema.createTable('m1', (t) => { t.integer('id').primary(); t.string('v'); }), down: (k) => k.schema.dropTable('m1') }
          : { up: (k) => k.schema.alterTable('m1', (t) => { t.integer('extra'); }), down: async () => {} } };
      const cfg2 = { migrationSource: mig };
      await knex.migrate.latest(cfg2);
      assert(await knex.schema.hasTable('m1') && await knex.schema.hasColumn('m1', 'extra'), 'migration effects');
      assert((await knex.migrate.currentVersion(cfg2)) !== 'none', 'version');
      await knex.migrate.rollback(cfg2, true);
    });
    await C('schema.dropTable', async () => { await knex.schema.dropTable('comments'); assert(!(await knex.schema.hasTable('comments')), 'still there'); });
  } finally { await knex.destroy().catch(() => {}); }
}

(async () => {
  const { NOVA_USER: user, NOVA_PASS: password, NOVA_MYSQL_PORT: myp, NOVA_PG_PORT: pgp } = process.env;
  for (const d of ['knex_mysql', 'knex_pg', 'knex_pg2']) await mkdb(d);
  await app('mysql2', { client: 'mysql2', connection: { host: '127.0.0.1', port: +myp, user, password, database: 'knex_mysql', charset: 'utf8mb4' }, pool: { min: 1, max: 1 }, acquireConnectionTimeout: 5000 }, KNOWN_MYSQL);
  const pgc = (db, extra = {}) => ({ client: 'pg', connection: { host: '127.0.0.1', port: +pgp, user, password, database: db, ssl: false }, pool: { min: 1, max: 1 }, acquireConnectionTimeout: 5000, ...extra });
  await app('pg', pgc('knex_pg'), KNOWN_PG);
  // Diagnostic pass: bypass knex's `select version()` probe (explicit server version) to see what else breaks.
  await app('pg-noversion', pgc('knex_pg2', { version: '15.0' }), KNOWN_PG_NV);
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });

const KNOWN_MYSQL = {}; // MY-1 fixed: ADD CONSTRAINT .. FOREIGN KEY is applied by a table rebuild
// PG-1..PG-3, PG-4, PG-7, PG-9..PG-11 fixed: no known failures remain on PostgreSQL.
const KNOWN_PG = {};
const KNOWN_PG_NV = {};
