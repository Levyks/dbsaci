"""Load the abstraction.mdc test schema (its `src/test/resources/data.sql`,
plain Oracle DDL) into pgSaci, so the Java integration suite has its tables.

Runs each statement over python-oracledb **thin** against pgSaci — same path the
app's ojdbc driver uses — so pgSaci's DDL translation is what actually creates
the Postgres tables. Idempotent: every table is dropped first.

Env:
  PGSACI_HOST / PGSACI_PORT / PGSACI_SERVICE / PGSACI_USER / PGSACI_PASS
  ABS_MDC_DIR   path to the abstraction.mdc project (default: the sibling
                checkout under ~/dev/eletra/is/abstraction/abstraction.mdc)
"""
import os
import re
import sys
from pathlib import Path

import oracledb  # thin mode; no Instant Client

HOST = os.environ.get("PGSACI_HOST", "127.0.0.1")
PORT = int(os.environ.get("PGSACI_PORT", "15301"))
SVC = os.environ.get("PGSACI_SERVICE", "XE")
USER = os.environ.get("PGSACI_USER", "hexing")
PASS = os.environ.get("PGSACI_PASS", "hexing")

ABS_DIR = Path(os.environ.get(
    "ABS_MDC_DIR",
    Path.home() / "dev" / "eletra" / "is" / "abstraction" / "abstraction.mdc",
))
DATA_SQL = ABS_DIR / "src" / "test" / "resources" / "data.sql"


def split_statements(text: str):
    # data.sql is simple: statements terminated by ';' at end of line, no
    # PL/SQL blocks. Strip line comments, then split on ';'.
    lines = []
    for ln in text.splitlines():
        s = ln.strip()
        if s.startswith("--"):
            continue
        lines.append(ln)
    blob = "\n".join(lines)
    return [s.strip() for s in blob.split(";") if s.strip()]


def table_name(create_stmt: str):
    m = re.match(
        r'\s*CREATE\s+TABLE\s+("?[A-Za-z_][A-Za-z0-9_$]*"?)',
        create_stmt, re.IGNORECASE,
    )
    return m.group(1) if m else None


def main():
    if not DATA_SQL.is_file():
        print(f"!! data.sql not found at {DATA_SQL}", file=sys.stderr)
        return 2
    stmts = split_statements(DATA_SQL.read_text(encoding="utf-8"))
    conn = oracledb.connect(user=USER, password=PASS,
                            dsn=f"{HOST}:{PORT}/{SVC}")
    cur = conn.cursor()

    # drop first (reverse order is fine — no FKs in data.sql)
    for st in stmts:
        t = table_name(st)
        if not t:
            continue
        try:
            cur.execute(f"DROP TABLE {t}")
        except Exception:
            pass

    made = 0
    for st in stmts:
        try:
            cur.execute(st)
            made += 1
        except Exception as e:  # noqa: BLE001
            head = " ".join(st.split())[:90]
            print(f"!! FAILED: {head}\n   {str(e).splitlines()[0]}",
                  file=sys.stderr)
            return 1
    conn.commit()
    cur.close()
    conn.close()
    print(f"schema loaded: {made} statements")
    return 0


if __name__ == "__main__":
    sys.exit(main())
