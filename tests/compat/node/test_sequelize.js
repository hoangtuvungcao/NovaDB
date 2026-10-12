'use strict';
// Sequelize v6 ORM mini-app over mysql2 and pg.
const path = require('path');
const { makeSuite, mkdb, HOSTILE, VIET, BLOB, assert, eq } = require(path.join(__dirname, '..', 'lib', 'harness.js'));
const { Sequelize, DataTypes, Op, QueryTypes } = require('sequelize');

async function app(label, dialect, db, known, explicitIds = false) {
  const S = makeSuite(`node-sequelize:${label}`);
  const C = (n, f) => S.run(n, f, known[n] || known['*'], !known[n]);
  const { NOVA_USER: user, NOVA_PASS: password } = process.env;
  const port = +(dialect === 'mysql' ? process.env.NOVA_MYSQL_PORT : process.env.NOVA_PG_PORT);
  const seq = new Sequelize(db, user, password, { host: '127.0.0.1', port, dialect, logging: false,
    pool: { max: 2, min: 1, acquire: 8000 }, dialectOptions: dialect === 'postgres' ? { ssl: false } : {} });
  try {
    if (!await C('authenticate', () => seq.authenticate())) return;
    // explicitIds: diagnostic variant. Ids are assigned client-side so that one bug (INSERT .. VALUES (DEFAULT, ..))
    // does not hide every other case. The real autoincrement variant is always run as well.
    let counter = 0;
    const idCol = () => explicitIds ? { type: DataTypes.INTEGER, primaryKey: true, defaultValue: () => ++counter }
                                    : { type: DataTypes.INTEGER, primaryKey: true, autoIncrement: true };
    const User = seq.define('User', {
      id: idCol(),
      email: { type: DataTypes.STRING(255), allowNull: false, unique: true },
      name: { type: DataTypes.STRING(200), allowNull: false },
      profile: { type: DataTypes.TEXT }, meta: { type: DataTypes.JSON }, avatar: { type: DataTypes.BLOB },
    }, { tableName: 'users', updatedAt: false });
    const Post = seq.define('Post', {
      id: idCol(),
      title: { type: DataTypes.STRING(300), allowNull: false }, body: DataTypes.TEXT,
      views: { type: DataTypes.INTEGER, allowNull: false, defaultValue: 0 },
    }, { tableName: 'posts', timestamps: false });
    const Comment = seq.define('Comment', {
      id: idCol(), body: { type: DataTypes.TEXT, allowNull: false },
    }, { tableName: 'comments', timestamps: false });
    User.hasMany(Post, { foreignKey: { name: 'userId', allowNull: false }, onDelete: 'CASCADE' }); Post.belongsTo(User, { foreignKey: 'userId' });
    Post.hasMany(Comment, { foreignKey: { name: 'postId', allowNull: false }, onDelete: 'CASCADE' }); Comment.belongsTo(Post, { foreignKey: 'postId' });
    Comment.belongsTo(User, { foreignKey: 'authorId' });

    if (!await C('sync() creates tables (FK, unique, defaults)', () => seq.sync({ force: true }))) return;
    await C('describeTable / showAllSchemas', async () => {
      const d = await seq.getQueryInterface().describeTable('users');
      assert(d.email && d.name, 'describeTable users: ' + JSON.stringify(Object.keys(d)));
      const t = await seq.getQueryInterface().showAllTables();
      assert(t.map((x) => (x.tableName || x)).includes('posts'), 'showAllTables ' + JSON.stringify(t));
    });
    const ids = {};
    await C('create + auto-increment id', async () => {
      const a = await User.create({ email: 'ann@example.com', name: 'Ann', profile: JSON.stringify({ k: [1, 2] }) });
      const b = await User.create({ email: 'bob@example.com', name: 'Bob' });
      assert(Number.isInteger(+a.id) && +b.id === +a.id + 1, `ids ${a.id} ${b.id}`);
      ids.a = +a.id; ids.b = +b.id;
    });
    await C('timestamps default (createdAt)', async () => {
      const u = await User.findByPk(ids.a); assert(u.createdAt && !isNaN(new Date(u.createdAt)), 'createdAt ' + u.createdAt);
    });
    await C('unique violation -> UniqueConstraintError', async () => {
      try { await User.create({ email: 'ann@example.com', name: 'dup' }); } catch (e) {
        assert(e.name === 'SequelizeUniqueConstraintError', `${e.name}: ${e.message}`); return;
      }
      throw new Error('duplicate accepted');
    });
    await C('FK violation -> ForeignKeyConstraintError', async () => {
      try { await Post.create({ title: 'orphan', userId: 99999 }); } catch (e) {
        assert(e.name === 'SequelizeForeignKeyConstraintError', `${e.name}: ${e.message}`); return;
      }
      throw new Error('FK violation accepted');
    });
    await C('managed transaction commit + rollback', async () => {
      await seq.transaction(async (t) => { await Post.create({ title: 'keep', userId: ids.a }, { transaction: t }); });
      try { await seq.transaction(async (t) => { await Post.create({ title: 'gone', userId: ids.a }, { transaction: t }); throw new Error('boom'); }); }
      catch (e) { assert(e.message === 'boom', e.message); }
      eq((await Post.findAll({ attributes: ['title'], raw: true })).map((r) => r.title), ['keep']);
    });
    await C('unmanaged transaction rollback()', async () => {
      const t = await seq.transaction();
      try { await Post.create({ title: 'tmp', userId: ids.a }, { transaction: t }); } finally { await t.rollback(); }
      assert((await Post.count({ where: { title: 'tmp' } })) === 0, 'rolled-back row visible');
    });
    await C('bulkCreate(500)', async () => {
      await Post.bulkCreate(Array.from({ length: 500 }, (_, i) => ({ userId: ids.b, title: `bulk-${String(i).padStart(4, '0')}`, body: 'x'.repeat(i % 30), views: i })));
      assert((await Post.count({ where: { title: { [Op.like]: 'bulk-%' } } })) === 500, 'count');
    });
    await C('pagination findAndCountAll', async () => {
      const r = await Post.findAndCountAll({ where: { title: { [Op.like]: 'bulk-%' } }, order: [['title', 'ASC']], limit: 10, offset: 20, raw: true });
      assert(r.count === 500 && r.rows.length === 10 && r.rows[0].title === 'bulk-0020' && r.rows[9].title === 'bulk-0029', JSON.stringify(r).slice(0, 200));
    });
    await C('eager loading (include) + aggregates', async () => {
      const p = await Post.findOne({ where: { title: 'keep' } });
      await Comment.create({ body: 'hi', postId: p.id, authorId: ids.b });
      const u = await User.findOne({ where: { email: 'ann@example.com' }, include: [{ model: Post, include: [Comment] }] });
      assert(u.Posts.length === 1 && u.Posts[0].Comments.length === 1 && u.Posts[0].Comments[0].body === 'hi', 'include tree');
      const sum = await Post.sum('views', { where: { title: { [Op.like]: 'bulk-%' } } });
      assert(Number(sum) === 124750, 'sum ' + sum);
      const grp = await Post.findAll({ attributes: ['userId', [seq.fn('COUNT', seq.col('id')), 'n']], group: ['userId'], having: seq.literal('COUNT(id) > 100'), raw: true });
      assert(grp.length === 1 && +grp[0].n === 500, JSON.stringify(grp));
    });
    await C('JSON data type', async () => {
      const u = await User.create({ email: 'j@x', name: 'J', meta: { a: [1, { b: VIET }], q: HOSTILE[0] } });
      const r = await User.findByPk(u.id); assert(r.meta && r.meta.a[1].b === VIET && r.meta.q === HOSTILE[0], JSON.stringify(r.meta));
    });
    await C('hostile strings (create + where + raw replacements)', async () => {
      for (let i = 0; i < HOSTILE.length; i++) await User.create({ email: `h${i}@x`, name: HOSTILE[i], profile: HOSTILE[i] });
      const got = (await User.findAll({ where: { email: { [Op.like]: 'h%@x' } }, order: [['id', 'ASC']], raw: true })).map((r) => r.name);
      eq(got, HOSTILE);
      const n = await seq.query('SELECT COUNT(*) AS n FROM users WHERE name = :n', { replacements: { n: HOSTILE[0] }, type: QueryTypes.SELECT });
      assert(Number(n[0].n) === 1, 'raw replacement count');
      const n2 = await seq.query('SELECT COUNT(*) AS n FROM users WHERE name = ?', { bind: undefined, replacements: [HOSTILE[1]], type: QueryTypes.SELECT });
      assert(Number(n2[0].n) === 1, 'positional');
      assert((await User.count()) >= 2 + HOSTILE.length, 'table intact');
    });
    await C('unicode Vietnamese + emoji', async () => {
      await User.create({ email: 'v@x', name: VIET });
      const r = await User.findOne({ where: { name: VIET } }); assert(r && r.name === VIET, 'mismatch');
    });
    await C('BLOB roundtrip', async () => {
      await User.update({ avatar: BLOB }, { where: { id: ids.b } });
      const r = await User.findByPk(ids.b); assert(Buffer.isBuffer(r.avatar) && Buffer.compare(r.avatar, BLOB) === 0, 'blob len ' + (r.avatar && r.avatar.length));
    });
    await C('update/destroy affected rows', async () => {
      const [n] = await Post.update({ views: seq.literal('views + 1') }, { where: { title: { [Op.like]: 'bulk-%' } } });
      assert(n === 500, 'update ' + n);
      const d = await Post.destroy({ where: { title: { [Op.like]: 'bulk-%' } } }); assert(d === 500, 'destroy ' + d);
    });
    await C('upsert()', async () => {
      await User.upsert({ email: 'ann@example.com', name: 'Ann2', id: ids.a });
      assert((await User.findByPk(ids.a)).name === 'Ann2', 'upsert did not update');
    });
    await C('cascade delete (ON DELETE CASCADE)', async () => {
      await User.destroy({ where: { id: ids.a } });
      assert((await Post.count({ where: { userId: ids.a } })) === 0, 'posts of deleted user remain');
    });
  } finally { await seq.close().catch(() => {}); }
}

(async () => {
  for (const d of ['seq_mysql', 'seq_mysql2', 'seq_pg']) await mkdb(d);
  await app('mysql2', 'mysql', 'seq_mysql', KNOWN_MYSQL);
  await app('mysql2-explicit-ids', 'mysql', 'seq_mysql2', KNOWN_MYSQL_EI, true);
  await app('pg', 'postgres', 'seq_pg', KNOWN_PG);
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });

// MY-2 (DEFAULT in VALUES) and MY-3 (JSON column type) fixed: no known failures remain on MySQL.
const KNOWN_MYSQL = {};
const KNOWN_MYSQL_EI = {};
// PG-12 fixed: SET / multi-statement simple queries are handled.
const KNOWN_PG = {};
