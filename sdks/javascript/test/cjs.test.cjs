const { test } = require("node:test");
const assert = require("node:assert/strict");
const { NovaDB, bindParams } = require("../dist/cjs/index.js");

test("CommonJS build loads", () => {
  assert.equal(typeof NovaDB, "function");
  assert.equal(bindParams("SELECT ?", [1]), "SELECT 1");
});
