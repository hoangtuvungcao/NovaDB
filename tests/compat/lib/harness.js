'use strict';
// Shared helpers for Node suites. Emits RESULT|suite|case|STATUS|detail lines.
const http = require('http');

function emit(suite, name, status, detail = '') {
  detail = String(detail).replace(/\s+/g, ' ').replace(/\|/g, '/').slice(0, 400);
  console.log(`RESULT|${suite}|${name}|${status}|${detail}`);
}

function makeSuite(name) {
  return {
    name,
    async run(caseName, fn, known, soft) {
      try {
        await fn();
      } catch (e) {
        const msg = `${e && e.name || 'Error'}: ${e && (e.sqlMessage || e.message) || e}`;
        if (known) emit(name, caseName, 'KNOWN-FAIL', `${known} || ${msg}`);
        else emit(name, caseName, 'FAIL', msg);
        return false;
      }
      emit(name, caseName, 'PASS', known && !soft ? 'now passes; remove known marker' : '');
      return true;
    },
    skip(caseName, why) { emit(name, caseName, 'SKIP', why); },
    emit(caseName, status, detail) { emit(name, caseName, status, detail); },
  };
}

function mkdb(id) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port: process.env.NOVA_HTTP_PORT, method: 'POST',
      path: `/v1/admin/databases/${id}` }, (res) => { res.resume(); res.on('end', () => res.statusCode < 300 ? resolve(id) : reject(new Error('mkdb ' + res.statusCode))); });
    req.on('error', reject); req.end();
  });
}

const HOSTILE = [
  "'); DROP TABLE users; --",
  "Robert'); DROP TABLE users;--",
  '" OR "1"="1',
  "a\\'b\\\\c\\nd",
  "x' UNION SELECT 1,2,3 --",
  "%_ ? :name $1 /* c */ -- tail",
  "line1\nline2\r\n\ttab",
];
const VIET = 'Nguyễn Văn Ạnh — Đặng Thị Hương 🙂 Tiếng Việt: ắằẳẵặ ếềểễệ ốồổỗộ ơờởỡợ ưừửữự';
const BLOB = Buffer.concat([Buffer.from(Array.from({ length: 256 }, (_, i) => i)), Buffer.from([0, 255, 0, 39, 34, 92, 59, 45, 45])]);

function assert(cond, msg) { if (!cond) throw new Error(msg || 'assertion failed'); }
function eq(a, b, msg) {
  const x = JSON.stringify(a), y = JSON.stringify(b);
  if (x !== y) throw new Error(`${msg || 'not equal'}: got ${x.slice(0, 200)} expected ${y.slice(0, 200)}`);
}

module.exports = { emit, makeSuite, mkdb, HOSTILE, VIET, BLOB, assert, eq };
