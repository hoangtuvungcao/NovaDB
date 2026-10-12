"""Tiny test harness shared by the Python suites. Emits RESULT|suite|case|STATUS|detail lines."""
import traceback

class Suite:
    def __init__(self, name):
        self.name = name

    def emit(self, case, status, detail=""):
        detail = " ".join(str(detail).split())[:400].replace("|", "/")
        print(f"RESULT|{self.name}|{case}|{status}|{detail}", flush=True)

    def case(self, name, fn, known=None, soft=False):
        """Run fn(). If it raises and `known` is given -> KNOWN-FAIL(known) else FAIL.
        If a KNOWN case passes it is reported as PASS with a note so the table can be pruned."""
        try:
            fn()
        except BaseException as e:  # noqa
            msg = f"{type(e).__name__}: {e}"
            if known:
                self.emit(name, "KNOWN-FAIL", f"{known} || {msg}")
            else:
                self.emit(name, "FAIL", msg)
            return False
        self.emit(name, "PASS", "now passes; remove known marker" if known and not soft else "")
        return True

    def skip(self, name, why):
        self.emit(name, "SKIP", why)


import os, urllib.request

def env(name):
    return os.environ[name]

def mkdb(name):
    """Create/open a managed NovaDB database through the HTTP admin API."""
    req = urllib.request.Request(f"http://127.0.0.1:{env('NOVA_HTTP_PORT')}/v1/admin/databases/{name}", method="POST")
    urllib.request.urlopen(req, timeout=10).read()
    return name

HOSTILE = [
    "'); DROP TABLE users; --",
    "Robert'); DROP TABLE users;--",
    "\" OR \"1\"=\"1",
    "a\\'b\\\\c\\nd",
    "x' UNION SELECT 1,2,3 --",
    "%_ ? :name $1 /* c */ -- tail",
    "line1\nline2\r\n\ttab",
]
VIET = "Nguyễn Văn Ạnh — Đặng Thị Hương 🙂 Tiếng Việt: ắằẳẵặ ếềểễệ ốồổỗộ ơờởỡợ ưừửữự"
BLOB = bytes(range(256)) + b"\x00\xff\x00'\"\\;--"
