# -*- coding: UTF-8 -*-
"""Drive upstream gStore's Python SDK (src/GstoreConnector.py, unmodified)
against a running gserver and check every answer.

Not an upstream file: scripts/conformance.sh runs it after upstream's own
examples (example/GET-example.py, example/POST-example.py), which only print
the answers. Each SDK method is called the way GstoreConnector sends it -- GET
(URL-encoded query string) or POST (JSON body) -- and the answer's StatusCode
and fields are checked.

usage: python3 sdk_check.py PORT
  The server's working directory must hold data/small/small.nt and
  data/small/small_add.nt (upstream's GSTORE_HOME layout), readable by build
  and batchInsert/batchRemove.

Exit status 0 when every check passes; the failed checks are listed.
"""
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "src"))
import GstoreConnector  # noqa: E402

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
DB = "sdk_small"
DATA = "data/small/small.nt"
ADD = "data/small/small_add.nt"

failures = []
passed = 0


def check(name, text, code=0, **fields):
    """The answer `text` (a JSON document) must have StatusCode `code` and
    the given field values (a callable `code` or value is a predicate)."""
    global passed
    try:
        j = json.loads(text)
    except ValueError as e:
        failures.append("%s: not JSON (%s): %.200s" % (name, e, text))
        return {}
    problems = []
    status = j.get("StatusCode")
    if not (code(status) if callable(code) else status == code):
        problems.append("StatusCode %r, expected %s" % (status, "another" if callable(code) else code))
    for k, want in fields.items():
        got = j.get(k)
        ok = want(got) if callable(want) else got == want
        if not ok:
            problems.append("%s = %r" % (k, got))
    if problems:
        failures.append("%s: %s -- %.300s" % (name, "; ".join(problems), text))
    else:
        passed += 1
    return j


def rows(j):
    return j.get("results", {}).get("bindings", [])


gc = GstoreConnector.GstoreConnector("localhost", PORT, "root", "123456")
bad = GstoreConnector.GstoreConnector("127.0.0.1", PORT, "root", "wrong-password")

# ---- system ----------------------------------------------------------------
check("check (GET)", gc.check())
check("check (POST)", gc.check("POST"))
check("login", gc.login("root", "123456"), CoreVersion=lambda v: bool(v))
check("login with a wrong password", bad.login("root", "x"), code=1001)
check("testConnect (GET)", gc.testConnect(), CoreVersion=lambda v: bool(v))
check("getCoreVersion (GET)", gc.getCoreVersion(), CoreVersion=lambda v: bool(v))
check("getCoreVersion (POST)", gc.getCoreVersion("POST"))
check("ipmanage type 1 (getBlackWhiteIP)", gc.getBlackWhiteIP())
check("stat", gc.stat())

# ---- database lifecycle ----------------------------------------------------
gc.drop(DB, False)  # a leftover of an earlier run, if any
check("build", gc.build(DB, DATA), success_num=25)
check("build an existing database is refused", gc.build(DB, DATA), code=1004)
check("load", gc.load(DB, "0"))
check("monitor", gc.monitor(DB), tripleNum=lambda n: str(n) == "25")
j = check("show", gc.show())
if not any(d.get("database") == DB for d in j.get("ResponseBody", [])):
    failures.append("show: %s not listed" % DB)

# ---- queries: POST (JSON body) and GET (URL-encoded) ------------------------
q = 'select ?s where { ?s <name> "Bookug Lobert" . }'
for how in ("POST", "GET"):
    j = check("query (%s)" % how, gc.query(DB, q, "json", how), AnsNum=1)
    if [b["s"]["value"] for b in rows(j)] != ["root"]:
        failures.append("query (%s): rows %r" % (how, rows(j)))
# Characters a URL query string must escape: '#', '&', '+', '%', '"', unicode.
tricky = 'select ?o where { <root> <name> ?o . FILTER(?o != "a&b#c+d%25 é") }'
for how in ("POST", "GET"):
    check("query with URL-special characters (%s)" % how,
          gc.query(DB, tricky, "json", how), AnsNum=1)
check("ask", gc.query(DB, "ask { <root> <contain> <node0> }"), AnsNum=1)
check("query syntax error", gc.query(DB, "select where {"), code=lambda c: c not in (None, 0))
check("update through query", gc.query(DB, "insert data { <sdk> <p> <o> . }"), AnsNum=1)
check("delete through query", gc.query(DB, "delete data { <sdk> <p> <o> . }"), AnsNum=1)
# fquery is not an upstream operation (src/GRPC/grpc_operation.h): 1100.
out = os.path.join(os.getcwd(), "sdk_fquery.txt")
gc.fquery(DB, q, out)
check("fquery (not an upstream operation)", open(out).read(), code=1100)
os.remove(out)

# ---- batch operations --------------------------------------------------------
check("batchInsert", gc.batchInsert(DB, ADD), success_num=6)
check("batchRemove", gc.batchRemove(DB, ADD), success_num=6)

# ---- transactions ----------------------------------------------------------
j = check("begin", gc.begin(DB, "1"), TID=lambda t: bool(t))
tid = j.get("TID", "0")
check("tquery update", gc.tquery(DB, tid, "insert data { <t> <p> <o> . }"), AnsNum=1)
check("tquery read", gc.tquery(DB, tid, "select ?s where { ?s <p> <o> }"), AnsNum=1)
check("commit", gc.commit(DB, tid))
j = check("begin again", gc.begin(DB, "1"))
tid2 = j.get("TID", "0")
check("tquery delete", gc.tquery(DB, tid2, "delete data { <t> <p> <o> . }"), AnsNum=1)
check("rollback", gc.rollback(DB, tid2))
check("the rollback kept the committed triple",
      gc.query(DB, "select ?s where { ?s <p> <o> }"), AnsNum=1)
check("txnlog", gc.getTransLog())
check("checkpoint", gc.checkpoint(DB))

# ---- backup / restore / export -----------------------------------------------
j = check("backup", gc.backup(DB), backupfilepath=lambda p: bool(p))
path = j.get("backupfilepath", "")
check("unload", gc.unload(DB))
check("restore", gc.restore(DB, path))
check("load after restore", gc.load(DB))
check("export", gc.exportDB(DB, "export/sdk"), filepath=lambda p: bool(p))

# ---- users and privileges ------------------------------------------------------
gc.usermanage("2", "sdk_user", "")  # a leftover of an earlier run, if any
check("usermanage add", gc.usermanage("1", "sdk_user", "sdk_pw_1"))
j = check("showuser", gc.showuser())
if not any(u.get("username") == "sdk_user" for u in j.get("ResponseBody", [])):
    failures.append("showuser: sdk_user not listed")
check("userprivilegemanage add query", gc.userprivilegemanage("1", "sdk_user", "1", DB))
user = GstoreConnector.GstoreConnector("127.0.0.1", PORT, "sdk_user", "sdk_pw_1")
check("the user may query", user.query(DB, q), AnsNum=1)
check("the user may not update", user.query(DB, "insert data { <u> <p> <o> }"),
      code=lambda c: c not in (None, 0))
check("usermanage delete", gc.usermanage("2", "sdk_user", ""))
check("the deleted user cannot log in", user.login("sdk_user", "sdk_pw_1"), code=1001)

# ---- cleanup ----------------------------------------------------------------
check("unload", gc.unload(DB))
check("drop", gc.drop(DB, False))

print("sdk_check: %d passed, %d failed" % (passed, len(failures)))
for f in failures:
    print("FAIL " + f)
sys.exit(1 if failures else 0)
