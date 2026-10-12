import datetime
import decimal
import unittest

from novadb import BindError, bind_params, hex_string, quote_identifier, quote_string, to_sql_literal


class LiteralTests(unittest.TestCase):
    def test_strings(self):
        self.assertEqual(quote_string("it's"), "'it''s'")
        self.assertEqual(to_sql_literal("O'Brien", "quote"), "'O''Brien'")
        self.assertEqual(hex_string("A'é"), "CAST(X'4127C3A9' AS TEXT)")
        self.assertEqual(to_sql_literal("x"), "CAST(X'78' AS TEXT)")

    def test_scalars(self):
        self.assertEqual(to_sql_literal(None), "NULL")
        self.assertEqual(to_sql_literal(True), "1")
        self.assertEqual(to_sql_literal(False), "0")
        self.assertEqual(to_sql_literal(42), "42")
        self.assertEqual(to_sql_literal(-3), "(-3)")
        self.assertEqual(to_sql_literal(1.5), "1.5")
        self.assertEqual(to_sql_literal(-0.0), "0.0")
        self.assertEqual(to_sql_literal(1e300), "1e+300")
        self.assertEqual(to_sql_literal(decimal.Decimal("-1.25")), "(-1.25)")
        self.assertEqual(to_sql_literal(b"\x00\xff"), "X'00FF'")
        self.assertEqual(to_sql_literal(bytearray(b"")), "X''")
        self.assertEqual(to_sql_literal(datetime.date(2026, 1, 2), "quote"), "'2026-01-02'")

    def test_rejects_unsupported(self):
        for bad in [float("nan"), float("inf"), {}, [1], object(), 2**63, "a\x00b", decimal.Decimal("NaN")]:
            with self.assertRaises(BindError, msg=repr(bad)):
                to_sql_literal(bad)


class BindTests(unittest.TestCase):
    def test_positional(self):
        self.assertEqual(bind_params("SELECT ?, ?, ?", [1, None, "a'b"], "quote"), "SELECT 1, NULL, 'a''b'")
        self.assertEqual(bind_params("SELECT x - ?", (-1,)), "SELECT x - (-1)")

    def test_named(self):
        self.assertEqual(
            bind_params("SELECT :a, :b_2, :a", {"a": 1, "b_2": "z"}, "quote"),
            "SELECT 1, 'z', 1",
        )
        self.assertEqual(bind_params("SELECT x::text, :v", {"v": 2}), "SELECT x::text, 2")
        with self.assertRaisesRegex(BindError, "missing value"):
            bind_params("SELECT :missing", {"a": 1})

    def test_ignores_literals_identifiers_comments(self):
        sql = "SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , ?"
        self.assertEqual(
            bind_params(sql, [5]),
            "SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , 5",
        )
        self.assertEqual(bind_params("SELECT 'it''s ?', ?", [1]), "SELECT 'it''s ?', 1")
        self.assertEqual(bind_params("SELECT ':x', :x", {"x": 3}), "SELECT ':x', 3")

    def test_count_mismatch(self):
        with self.assertRaisesRegex(BindError, "not enough"):
            bind_params("SELECT ?, ?", [1])
        with self.assertRaisesRegex(BindError, "too many"):
            bind_params("SELECT ?", [1, 2])
        with self.assertRaisesRegex(BindError, "sequence"):
            bind_params("SELECT ?", {"a": 1})
        with self.assertRaisesRegex(BindError, "numbered"):
            bind_params("SELECT ?1", [1])
        with self.assertRaises(BindError):
            bind_params("SELECT ?", "x")

    def test_no_params(self):
        self.assertEqual(bind_params("SELECT '?', ?"), "SELECT '?', ?")

    def test_injection_stays_in_literal(self):
        evil = "x'); DROP TABLE users; --"
        self.assertEqual(
            bind_params("INSERT INTO t VALUES (?)", [evil], "quote"),
            "INSERT INTO t VALUES ('x''); DROP TABLE users; --')",
        )
        self.assertRegex(bind_params("INSERT INTO t VALUES (?)", [evil]), r"^INSERT INTO t VALUES \(CAST\(X'[0-9A-F]+' AS TEXT\)\)$")

    def test_identifiers(self):
        self.assertEqual(quote_identifier("users"), '"users"')
        self.assertEqual(quote_identifier("main.users"), '"main"."users"')
        for bad in ['a"b', "1a", "a b", ""]:
            with self.assertRaises(BindError):
                quote_identifier(bad)


if __name__ == "__main__":
    unittest.main()
