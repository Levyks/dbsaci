"""OCI (thick-mode) smoke test against a running DbSaci.

Uses python-oracledb in **thick mode** — it loads the Oracle Instant Client
(`oci.dll` / `libclntsh`) and talks to DbSaci over the real OCI code path
instead of python-oracledb's own thin protocol implementation. This is the
"one OCI-based client" smoke from the alpha checklist.

Env:
  ORACLE_INSTANT_CLIENT  dir holding oci.dll / libclntsh (default: the Windows
                         install path; override on Linux/macOS)
  DBSACI_HOST/PORT/USER/PASSWORD/SERVICE  as the other probes

Exits non-zero on the first failed assertion or if the client library or a
connection cannot be established.
"""
import os
import sys

import oracledb

default_ic = r"C:\Program Files\Oracle\instantclient_19_32"
lib_dir = os.environ.get("ORACLE_INSTANT_CLIENT", default_ic)

try:
    oracledb.init_oracle_client(lib_dir=lib_dir if os.path.isdir(lib_dir) else None)
except Exception as e:  # noqa: BLE001
    print(f"could not initialise OCI client from {lib_dir!r}: {e}")
    sys.exit(3)

if not oracledb.is_thin_mode():
    print(f"OCI thick mode active (client {oracledb.clientversion()})")
else:
    print("still in thin mode — Instant Client not loaded")
    sys.exit(3)

host = os.environ.get("DBSACI_HOST", "127.0.0.1")
port = int(os.environ.get("DBSACI_PORT", "1521"))
user = os.environ.get("DBSACI_USER", "corpus")
password = os.environ.get("DBSACI_PASSWORD", "corpus")
service = os.environ.get("DBSACI_SERVICE", "FREEPDB1")
dsn = f"{host}:{port}/{service}"

checks = []


def check(name, got, want):
    ok = got == want
    checks.append(ok)
    print(("PASS " if ok else "FAIL "), name, "->", repr(got), "" if ok else f"(want {want!r})")


print(f"connecting (OCI) to {dsn} as {user}")
conn = oracledb.connect(user=user, password=password, dsn=dsn)
print("connected; server version:", conn.version)

cur = conn.cursor()
cur.execute("SELECT 1 FROM DUAL")
check("select_1_from_dual", cur.fetchone(), (1,))

cur.execute("SELECT name FROM people WHERE id = :pid", pid=1)
check("bind_select", cur.fetchone(), ("Ada",))

cur.execute("SELECT COUNT(*) FROM people")
check("count_people", cur.fetchone()[0] >= 1, True)

cur.close()
conn.close()

failed = checks.count(False)
print(f"\n{len(checks) - failed}/{len(checks)} checks passed")
sys.exit(1 if failed else 0)
