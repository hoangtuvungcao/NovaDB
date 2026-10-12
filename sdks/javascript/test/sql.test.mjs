import { test } from "node:test";
import assert from "node:assert/strict";
import { bindParams, toSqlLiteral, quoteString, hexString, quoteIdentifier, BindError } from "../dist/esm/index.js";

const q = { stringEncoding: "quote" };

test("string literals double single quotes", () => {
  assert.equal(quoteString("it's"), "'it''s'");
  assert.equal(quoteString("''"), "''''''");
  assert.equal(toSqlLiteral("O'Brien", q), "'O''Brien'");
});

test("hex string encoding is UTF-8", () => {
  assert.equal(hexString("A'é"), "CAST(X'4127C3A9' AS TEXT)");
  assert.equal(hexString(""), "CAST(X'' AS TEXT)");
  assert.equal(toSqlLiteral("x"), "CAST(X'78' AS TEXT)");
});

test("scalar literals", () => {
  assert.equal(toSqlLiteral(null), "NULL");
  assert.equal(toSqlLiteral(true), "1");
  assert.equal(toSqlLiteral(false), "0");
  assert.equal(toSqlLiteral(42), "42");
  assert.equal(toSqlLiteral(1.5), "1.5");
  assert.equal(toSqlLiteral(-7), "(-7)");
  assert.equal(toSqlLiteral(-0), "0");
  assert.equal(toSqlLiteral(9007199254740993n), "9007199254740993");
  assert.equal(toSqlLiteral(-5n), "(-5)");
  assert.equal(toSqlLiteral(new Uint8Array([0, 255, 16])), "X'00FF10'");
  assert.equal(toSqlLiteral(new Uint8Array([]).buffer), "X''");
  assert.equal(toSqlLiteral(new Date(Date.UTC(2026, 0, 2, 3, 4, 5))), hexString("2026-01-02T03:04:05.000Z"));
});

test("unsupported values are rejected", () => {
  for (const bad of [undefined, NaN, Infinity, {}, [1], () => 1, Symbol("s"), 2n ** 63n, "a\u0000b", new Date(NaN)]) {
    assert.throws(() => toSqlLiteral(bad), BindError, String(bad?.toString?.() ?? bad));
  }
});

test("positional placeholders", () => {
  assert.equal(bindParams("SELECT ?, ?, ?", [1, null, "a'b"], q), "SELECT 1, NULL, 'a''b'");
  assert.equal(bindParams("SELECT x - ?", [-1]), "SELECT x - (-1)");
});

test("named placeholders", () => {
  assert.equal(
    bindParams("SELECT * FROM t WHERE a = :a AND b = :b_2 OR a = :a", { a: 1, b_2: "z" }, q),
    "SELECT * FROM t WHERE a = 1 AND b = 'z' OR a = 1",
  );
  assert.equal(bindParams("SELECT x::text, :v", { v: 2 }), "SELECT x::text, 2");
  assert.throws(() => bindParams("SELECT :missing", { a: 1 }), /missing value/);
});

test("placeholders inside literals, identifiers and comments are ignored", () => {
  const sql = `SELECT '?', ':a', "col?", \`b?\`, [c?] -- what?
  /* :a ? */ , ? , :a`;
  assert.equal(
    bindParams(sql.replace(", :a", ""), [5]),
    `SELECT '?', ':a', "col?", \`b?\`, [c?] -- what?
  /* :a ? */ , 5 `,
  );
  assert.equal(bindParams("SELECT 'it''s ?', ?", [1]), "SELECT 'it''s ?', 1");
  assert.equal(bindParams("SELECT ':x', :x", { x: 3 }), "SELECT ':x', 3");
});

test("parameter count mismatches throw", () => {
  assert.throws(() => bindParams("SELECT ?, ?", [1]), /not enough/);
  assert.throws(() => bindParams("SELECT ?", [1, 2]), /too many/);
  assert.throws(() => bindParams("SELECT ?", { a: 1 }), /pass an array/);
  assert.throws(() => bindParams("SELECT ?1", [1]), /numbered/);
});

test("no params leaves SQL untouched", () => {
  assert.equal(bindParams("SELECT '?' , ?"), "SELECT '?' , ?");
  assert.equal(bindParams("SELECT 1", []), "SELECT 1");
});

test("injection attempts stay inside one literal", () => {
  const evil = "x'); DROP TABLE users; --";
  assert.equal(bindParams("INSERT INTO t VALUES (?)", [evil], q), "INSERT INTO t VALUES ('x''); DROP TABLE users; --')");
  assert.match(bindParams("INSERT INTO t VALUES (?)", [evil]), /^INSERT INTO t VALUES \(CAST\(X'[0-9A-F]+' AS TEXT\)\)$/);
});

test("identifiers", () => {
  assert.equal(quoteIdentifier("users"), '"users"');
  assert.equal(quoteIdentifier("main.users"), '"main"."users"');
  assert.throws(() => quoteIdentifier('a"b'), BindError);
  assert.throws(() => quoteIdentifier("1abc"), BindError);
  assert.throws(() => quoteIdentifier("a b"), BindError);
});
