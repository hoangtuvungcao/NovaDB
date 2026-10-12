'use strict';
// Prisma (mysql provider) against the NovaDB MySQL gateway. Optional suite: runs `prisma db push`; if the schema engine
// cannot even introspect the server the suite is reported as KNOWN-FAIL and the client checks are skipped.
const path = require('path');
const fs = require('fs');
const { execFileSync } = require('child_process');
const { makeSuite, mkdb, HOSTILE, VIET, assert } = require(path.join(__dirname, '..', 'lib', 'harness.js'));

(async () => {
  const S = makeSuite('node-prisma:mysql');
  const dir = process.env.PRISMA_DIR; // prepared by run.sh (npm install done there)
  if (!dir || !fs.existsSync(path.join(dir, 'node_modules', '.bin', 'prisma'))) { S.skip('prisma', 'prisma not installed (npm install failed or offline)'); return; }
  await mkdb('prisma_app');
  const env = { ...process.env, DATABASE_URL: `mysql://${process.env.NOVA_USER}:${process.env.NOVA_PASS}@127.0.0.1:${process.env.NOVA_MYSQL_PORT}/prisma_app`, CI: '1', PRISMA_HIDE_UPDATE_MESSAGE: '1' };
  const bin = path.join(dir, 'node_modules', '.bin', 'prisma');
  const schema = path.join(dir, 'schema.prisma');
  const known = null; // MY-4 fixed: system variables are typed; prisma db push + client CRUD pass
  const ok = await S.run('prisma db push (schema engine)', async () => {
    try { execFileSync(bin, ['db', 'push', '--schema', schema, '--skip-generate', '--accept-data-loss'], { env, cwd: dir, stdio: 'pipe', timeout: 80000 }); }
    catch (e) { throw new Error(String(e.stderr || e.stdout || e.message).split('\n').filter(Boolean).slice(-2).join(' ')); }
  }, known);
  if (!ok) return;
  await S.run('prisma generate', async () => { execFileSync(bin, ['generate', '--schema', schema], { env, cwd: dir, stdio: 'pipe', timeout: 80000 }); });
  const { PrismaClient, Prisma } = require(path.join(dir, 'node_modules', '@prisma', 'client'));
  const prisma = new PrismaClient({ datasources: { db: { url: env.DATABASE_URL } } });
  try {
    let u;
    await S.run('create + autoincrement + relations', async () => {
      u = await prisma.user.create({ data: { email: 'ann@example.com', name: 'Ann', posts: { create: [{ title: 'p1' }, { title: 'p2' }] } }, include: { posts: true } });
      assert(u.id > 0 && u.posts.length === 2, 'create nested');
    });
    await S.run('unique violation -> P2002', async () => {
      try { await prisma.user.create({ data: { email: 'ann@example.com', name: 'dup' } }); } catch (e) { assert(e.code === 'P2002', 'code ' + e.code); return; }
      throw new Error('duplicate accepted');
    });
    await S.run('$transaction rollback', async () => {
      try { await prisma.$transaction(async (tx) => { await tx.user.create({ data: { email: 'gone@x', name: 'g' } }); throw new Error('boom'); }); } catch (e) {}
      assert((await prisma.user.count({ where: { email: 'gone@x' } })) === 0, 'rolled back row visible');
    });
    await S.run('hostile + unicode strings', async () => {
      for (let i = 0; i < HOSTILE.length; i++) await prisma.user.create({ data: { email: `h${i}@x`, name: HOSTILE[i] } });
      await prisma.user.create({ data: { email: 'v@x', name: VIET } });
      const got = await prisma.user.findMany({ where: { email: { startsWith: 'h' } }, orderBy: { id: 'asc' } });
      assert(JSON.stringify(got.map((r) => r.name)) === JSON.stringify(HOSTILE), 'hostile mismatch');
      assert((await prisma.user.findFirst({ where: { name: VIET } })).name === VIET, 'unicode');
    });
    await S.run('pagination + aggregate', async () => {
      const page = await prisma.user.findMany({ orderBy: { id: 'asc' }, skip: 1, take: 3 }); assert(page.length === 3, 'page');
      const agg = await prisma.post.aggregate({ _count: true, _sum: { views: true } }); assert(agg._count === 2, 'agg');
    });
  } finally { await prisma.$disconnect(); }
})().catch((e) => { console.error(e); process.exit(1); });
