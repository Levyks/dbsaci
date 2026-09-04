"""Run the DbSaci compatibility corpus (`tests/corpus/*.sql`) through the OCI
thick client (python-oracledb thick mode).

This mirrors `tests/corpus.rs` — same golden grammar, same value rendering — but
drives the queries over the real Oracle Call Interface instead of oracle-rs, so
it exercises DbSaci's OCI wire path end to end.

Environment:
  DBSACI_HOST / DBSACI_PORT / DBSACI_USER / DBSACI_PASSWORD / DBSACI_SERVICE
  DBSACI_DB_HOST / DBSACI_DB_PORT / DBSACI_DB_NAME / DBSACI_DB_USER /
  DBSACI_DB_PASSWORD   direct TCP connection to the backing Postgres (for
                    fixtures, teardown, and `-- verify` on a second connection)
  ORACLE_INSTANT_CLIENT   Instant Client dir (default: Windows install path)
  CORPUS_FILTER     optional substring; only run groups/cases containing it

Exit code: number of failed cases (capped at 125), 0 if all pass.
"""
import datetime
import decimal
import os
import re
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import TimeoutError as FuturesTimeout
from pathlib import Path

# Serial pool: at most one case runs at a time. A timed-out case's thread is
# abandoned (still stuck), so allow a few spares before the process gives up.
_CASE_POOL = ThreadPoolExecutor(max_workers=8, thread_name_prefix="case")

import oracledb

try:
    import pg8000.native as _pg
except ImportError:  # self-bootstrap: the direct-PG driver for fixtures/verify
    import subprocess as _sp

    _sp.check_call([sys.executable, "-m", "pip", "install", "--quiet", "pg8000"])
    import pg8000.native as _pg

ROOT = Path(__file__).resolve().parents[2]
CORPUS_DIR = ROOT / "tests" / "corpus"

IC = os.environ.get("ORACLE_INSTANT_CLIENT", r"C:\Program Files\Oracle\instantclient_19_32")
try:
    oracledb.init_oracle_client(lib_dir=IC if os.path.isdir(IC) else None)
except Exception as e:  # noqa: BLE001
    print(f"could not init OCI client from {IC!r}: {e}")
    sys.exit(3)
if oracledb.is_thin_mode():
    print("still thin mode — Instant Client not loaded")
    sys.exit(3)

HOST = os.environ.get("DBSACI_HOST", "127.0.0.1")
PORT = int(os.environ.get("DBSACI_PORT", "1521"))
USER = os.environ.get("DBSACI_USER", "corpus")
PW = os.environ.get("DBSACI_PASSWORD", "corpus")
SVC = os.environ.get("DBSACI_SERVICE", "FREEPDB1")
DSN = f"{HOST}:{PORT}/{SVC}"
FILTER = os.environ.get("CORPUS_FILTER", "")

PG_HOST = os.environ.get("DBSACI_DB_HOST", "127.0.0.1")
PG_PORT = int(os.environ.get("DBSACI_DB_PORT", "5432"))
PG_DB = os.environ.get("DBSACI_DB_NAME", "postgres")
PG_USER = os.environ.get("DBSACI_DB_USER", "corpus")
PG_PW = os.environ.get("DBSACI_DB_PASSWORD", "corpus")


def _pg_cell(v) -> str:
    """Render one value the way `psql -t -A` does: NULL -> empty, bool -> t/f."""
    if v is None:
        return ""
    if isinstance(v, bool):
        return "t" if v else "f"
    return str(v)


# a genuine SQL error names itself and will not fix itself on retry
_HARD_ERR = ("syntax error", "does not exist", "already exists", "violates",
             "duplicate key", "cannot ", "permission denied", "out of range",
             "invalid input", "division by zero", "not-null constraint")


def psql(sql: str, tolerant: bool = False) -> str:
    """Run one SQL statement on the backing Postgres over a direct TCP
    connection (fixtures / verify / teardown).

    This used to shell out to `docker exec ... psql`, which flaked and *hung*
    under load — and `subprocess`'s reaper `join()` then blocked forever,
    wedging the whole run. A direct socket cannot wedge that way.

    The remaining hang risk is a teardown `DROP` blocking on a lock the open
    OCI connection still holds (its last case has not been rolled back yet);
    pg8000's `timeout=` only covers connect, not later reads. So the
    connection is opened with server-side `lock_timeout` / `statement_timeout`
    GUCs: a blocked statement is cancelled by Postgres instead of hanging,
    the error is retried a few times, and a still-stuck teardown is tolerated."""
    err = None
    for attempt in range(4):
        con = None
        try:
            con = _pg.Connection(
                user=PG_USER, host=PG_HOST, port=PG_PORT,
                database=PG_DB, password=PG_PW, timeout=15,
                startup_params={
                    "lock_timeout": "8000",
                    "statement_timeout": "25000",
                    "idle_in_transaction_session_timeout": "15000",
                },
            )
            rows = con.run(sql)
            if not rows:
                return ""
            return "\n".join(
                "\t".join(_pg_cell(c) for c in r) for r in rows)
        except Exception as e:  # noqa: BLE001
            err = e
            if any(k in str(e).lower() for k in _HARD_ERR):
                break
            time.sleep(0.5 * (attempt + 1))
        finally:
            if con is not None:
                try:
                    con.close()
                except Exception:  # noqa: BLE001
                    pass
    if not tolerant:
        raise RuntimeError(f"psql failed: {err}")
    return ""


# --------------------------------------------------------------------------
# golden-file model
# --------------------------------------------------------------------------
class Case:
    __slots__ = ("name", "setup", "binds", "sql", "kind", "payload",
                 "teardown", "verify", "tag")

    def __init__(self):
        self.name = ""
        self.setup = []
        self.binds = []
        self.sql = ""
        self.kind = None        # rows | regex | rows_exactly | rowcount | error | ok
        self.payload = None
        self.teardown = []
        self.verify = None      # (sql, expected)
        self.tag = None


def directive(line, key):
    pfx = f"-- {key}:"
    return line[len(pfx):].strip() if line.startswith(pfx) else None


def load_group(path: Path):
    fixtures, cases = [], []
    # The corpus files are UTF-8; `Path.read_text()` would otherwise decode with
    # the platform default (cp1252 on Windows), turning `café` into `cafÃ©` and
    # e.g. `LENGTH('café')` into 5.
    lines = path.read_text(encoding="utf-8").splitlines()
    i, n = 0, len(lines)
    while i < n:
        raw = lines[i]
        line = raw.strip()
        i += 1
        if not line or line.startswith("#"):
            continue
        fx = directive(line, "fixture")
        if fx is not None:
            fixtures.append(fx)
            continue
        if not line.startswith("-- case:"):
            raise RuntimeError(f"{path.name}: unexpected line outside a case: {raw!r}")
        c = Case()
        c.name = line[len("-- case:"):].strip()
        sql_lines = []
        while i < n:
            l = lines[i].rstrip("\n")
            s = l.strip()
            i += 1
            if s == "-- end":
                break
            v = directive(s, "setup")
            if v is not None:
                c.setup.append(v)
                continue
            v = directive(s, "setup?")
            if v is not None:
                c.setup.append("\0" + v)
                continue
            v = directive(s, "bind")
            if v is not None:
                c.binds.append(v)
                continue
            v = directive(s, "tag")
            if v is not None:
                c.tag = v
                continue
            v = directive(s, "teardown")
            if v is not None:
                c.teardown.append(v)
                continue
            v = directive(s, "verify")
            if v is not None:
                lhs, rhs = v.split("=>", 1)
                c.verify = (lhs.strip(), rhs.strip())
                continue
            v = directive(s, "expect-regex")
            if v is not None:
                c.kind, c.payload = "regex", v
                continue
            v = directive(s, "rows")
            if v is not None:
                c.kind, c.payload = "rows_exactly", int(v)
                continue
            v = directive(s, "rowcount")
            if v is not None:
                c.kind, c.payload = "rowcount", int(v)
                continue
            v = directive(s, "error")
            if v is not None:
                c.kind, c.payload = "error", v.strip()
                continue
            if s == "-- ok":
                c.kind = "ok"
                continue
            if s == "-- expect:":
                block = []
                while i < n and lines[i].strip() != "-- end":
                    block.append(lines[i].rstrip("\n"))
                    i += 1
                # consume "-- end"
                if i < n:
                    i += 1
                c.kind, c.payload = "rows", [b.strip() for b in block if b.strip() != ""] \
                    if any(b.strip() for b in block) else []
                # exact block (may legitimately be empty = no rows)
                c.payload = [b for b in ("\n".join(block)).split("\n")] if block else []
                c.payload = [b.strip() for b in block]
                break
            # else: SQL body line
            sql_lines.append(l)
        c.sql = "\n".join(x for x in sql_lines).strip()
        cases.append(c)
    return fixtures, cases


# --------------------------------------------------------------------------
# value rendering — must match tests/corpus.rs format_value / format_rows
# --------------------------------------------------------------------------
def fmt_number(x):
    if isinstance(x, bool):
        return "true" if x else "false"
    if isinstance(x, int):
        return str(x)
    if isinstance(x, float):
        if x == int(x) and abs(x) < 1e16:
            return str(int(x))
        return repr(x)
    if isinstance(x, decimal.Decimal):
        s = format(x, "f")
        if "." in s:
            s = s.rstrip("0").rstrip(".")
        return s or "0"
    return str(x)


def fmt_dt(dt):
    has_tz = dt.tzinfo is not None and dt.utcoffset() is not None
    mic48 = dt.microsecond
    midnight = dt.hour == 0 and dt.minute == 0 and dt.second == 0 and mic48 == 0
    if midnight and not has_tz:
        s = f"{dt.year:04d}-{dt.month:02d}-{dt.day:02d}"
    else:
        s = f"{dt.year:04d}-{dt.month:02d}-{dt.day:02d} {dt.hour:02d}:{dt.minute:02d}:{dt.second:02d}"
    if mic48 > 0:
        s += f".{mic48:06d}".rstrip("0")
    if has_tz:
        off = dt.utcoffset()
        total = int(off.total_seconds())
        sign = "-" if total < 0 else "+"
        total = abs(total)
        s += f" {sign}{total // 3600:02d}:{(total % 3600) // 60:02d}"
    return s


def fmt_value(v):
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (bytes, bytearray)):
        return "0x" + bytes(v).hex()
    if isinstance(v, (int, float, decimal.Decimal)):
        return fmt_number(v)
    if isinstance(v, datetime.datetime):
        return fmt_dt(v)
    if isinstance(v, datetime.date):
        return f"{v.year:04d}-{v.month:02d}-{v.day:02d}"
    if isinstance(v, oracledb.LOB):
        return v.read()
    return str(v)


def fmt_rows(rows):
    return [" | ".join(fmt_value(c) for c in row) for row in rows]


# --------------------------------------------------------------------------
# bind decoding — `-- bind: <type> <value>`
# --------------------------------------------------------------------------
def decode_binds(specs):
    out = []
    for spec in specs:
        ty, _, rest = spec.partition(" ")
        rest = rest.strip()
        if ty == "null":
            out.append(None)
        elif ty == "int":
            out.append(int(rest))
        elif ty == "float":
            out.append(float(rest))
        elif ty == "str":
            out.append(rest)
        elif ty == "bytes":
            out.append(bytes.fromhex(rest[2:] if rest.startswith("0x") else rest))
        elif ty == "date":
            d, _, t = rest.partition(" ")
            y, mo, da = (int(x) for x in d.split("-"))
            hh, mi, se = (int(x) for x in (t or "0:0:0").split(":"))
            out.append(datetime.datetime(y, mo, da, hh, mi, se))
        else:
            raise RuntimeError(f"unknown bind type {ty!r}")
    return out


def number_to_str(cursor, name, default_type, size, precision, scale):
    if default_type == oracledb.DB_TYPE_NUMBER:
        return cursor.var(str, arraysize=cursor.arraysize)


MUTATING_HEADS = {"INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER",
                  "MERGE", "TRUNCATE", "COMMIT", "ROLLBACK", "GRANT", "REVOKE",
                  "SET", "SAVEPOINT", "RELEASE"}


def case_mutates(c: Case):
    if c.setup or c.teardown or c.verify:
        return True
    if c.kind == "rowcount":
        return True
    head = c.sql.strip().split(None, 1)[0].upper() if c.sql.strip() else ""
    return head in MUTATING_HEADS


# Cases whose *expected* behaviour is a thin-driver / oracle-rs trait that the
# OCI client library itself contradicts before any bytes reach DbSaci. Each was
# re-verified (clients/oci/verify_oracle_incompatible.py, 2026-08-31) to raise the IDENTICAL error
# straight against a live Oracle XE 21c over python-oracledb **thick** — i.e.
# they do NOT pass "against a real oracle db" on this transport, so per the
# project goal they are legitimately skipped for the OCI probe only.
OCI_CLIENT_INCOMPATIBLE = {
    # The OCI bind layer validates placeholder count/names locally and raises
    # ORA-01036 ("illegal variable name/number") for a surplus / unreferenced
    # bind; thin silently ignores it. Live XE thick: ORA-01036 (confirmed).
    "binds::placeholder_inside_string_literal_is_not_a_bind",
    "binds::surplus_bind_values_are_ignored",
    # `RETURNING <col>` with no `INTO` is a hard Oracle syntax error.
    # Live XE thick: ORA-00925 "missing INTO keyword" (confirmed). DbSaci is
    # deliberately lenient (bare PG RETURNING); only thin/oracle-rs exercise it.
    "dml::update_with_returning_clause",
}

# Cases where the OCI thick client's TTC response parser deadlocks on the
# rapid CREATE FUNCTION + CREATE TRIGGER + INSERT frame sequence — it stops
# reading the socket and ignores `call_timeout`, so the run cannot recover.
# The identical wire (captured via a TCP tee) is byte-valid and every other
# probe (thin / ojdbc / ODP.NET) runs these; adding any inter-frame delay
# clears it intermittently. A client-library race, not a DbSaci wire bug.
OCI_CLIENT_HANGS = set()  # (was: triggers + bytes_bind — those were a DbSaci
# cursor-resolution bug, not a client hang; the `0x4e` re-execute of the
# runner's `SELECT 1` probe resolved to the previous query. Fixed in
# server.rs — the frame's named cursor id is now authoritative.)


def _force_close(conn):
    """Unblock a wedged main thread from the watchdog. `conn.cancel()` is
    non-blocking and aborts the in-flight OCI call (unlike `conn.close()`, which
    itself does a LOGOFF round trip and hangs on a dead connection)."""
    for attempt in (
        lambda: conn.cancel(),
        lambda: conn.close(),
    ):
        try:
            attempt()
        except Exception:  # noqa: BLE001
            pass


def _decimal_number_handler(cursor, name, default_type, size, precision, scale):
    # python-oracledb defaults NUMBER -> Python float, which loses precision on
    # values past ~15 significant digits (`98765432109876.54` -> `...55`).
    # oracle-rs / tests/corpus.rs render the exact decimal, so match that.
    if default_type == oracledb.DB_TYPE_NUMBER:
        return cursor.var(decimal.Decimal, arraysize=cursor.arraysize)
    return None


def connect():
    conn = oracledb.connect(user=USER, password=PW, dsn=DSN)
    conn.outputtypehandler = _decimal_number_handler
    # A single wedged case must not hang the whole run (mirrors the 20s guard in
    # tests/corpus.rs). call_timeout aborts the in-flight OCI round trip.
    try:
        conn.call_timeout = 7000
    except Exception:  # noqa: BLE001
        pass
    return conn


def run_query_all(conn, sql, binds):
    cur = conn.cursor()
    cur.execute(sql, binds)
    rows = cur.fetchall()
    cur.close()
    return rows


def main():
    files = sorted(CORPUS_DIR.glob("*.sql"))
    groups = []
    for f in files:
        stem = f.stem
        try:
            fixtures, cases = load_group(f)
        except Exception as e:  # noqa: BLE001
            print(f"!! parse {f.name}: {e}")
            continue
        groups.append((stem, fixtures, cases))

    # fixtures once, on the direct PG connection
    for stem, fixtures, _ in groups:
        for fx in fixtures:
            try:
                psql(fx)
            except Exception as e:  # noqa: BLE001
                print(f"!! fixture ({stem}): {e}")

    conn = connect()
    npass = nfail = nskip = 0
    failures = []

    for stem, _, cases in groups:
        for c in cases:
            full = f"{stem}::{c.name}"
            if FILTER and FILTER not in full:
                continue
            if (c.tag == "skip"
                    or full in OCI_CLIENT_INCOMPATIBLE
                    or full in OCI_CLIENT_HANGS):
                nskip += 1
                continue
            # The OCI thick client's response parser races when frames from
            # consecutive statements arrive with no gap (wedges hard on trigger
            # DDL — ignores `call_timeout`). A brief main-thread pause per case
            # lets its background I/O thread drain. `DBSACI_OCI_PACE_MS` tunes it.
            _pace = float(os.environ.get("DBSACI_OCI_PACE_MS", "30")) / 1000.0
            if _pace > 0:
                import time as _t
                _t.sleep(_pace)
            if os.environ.get("CORPUS_TRACE"):
                print(f">> {full}", file=sys.stderr, flush=True)
            # Run each case (and, below, the isolation probe) on a worker thread
            # with a hard wall-clock cap. A wedged OCI parse loop ignores
            # `call_timeout`; on timeout we abandon that connection (its worker
            # thread stays stuck) and rebuild, so the run always progresses.
            try:
                ok, msg = _CASE_POOL.submit(run_case, conn, c).result(timeout=45)
            except FuturesTimeout:
                ok, msg = False, "watchdog: case exceeded 45s (connection abandoned)"
                # Break *and* close so the abandoned worker thread's blocked
                # `fetchall()` actually unwinds and returns to the pool (8
                # permanently-stuck threads would exhaust it and wedge the run).
                for act in (conn.cancel, conn.close):
                    try:
                        act()
                    except Exception:  # noqa: BLE001
                        pass
                conn = None
            except Exception as e:  # noqa: BLE001
                ok, msg = False, f"runner exception: {type(e).__name__}: {e}"
            if os.environ.get("CORPUS_TRACE"):
                print(f"{'ok  ' if ok else 'FAIL'} {full}"
                      + ("" if ok else f": {msg.splitlines()[0] if msg else ''}"),
                      file=sys.stderr, flush=True)

            # isolation: reconnect after mutating / crashed / abandoned. The
            # probe itself can wedge, so it too runs under a wall-clock cap.
            crashed = conn is None
            if not crashed:
                def _probe(c):
                    cur = c.cursor()
                    cur.execute("SELECT 1 FROM DUAL")
                    cur.fetchall()
                    cur.close()

                try:
                    _CASE_POOL.submit(_probe, conn).result(timeout=10)
                except Exception:  # noqa: BLE001
                    crashed = True
                    try:
                        conn.cancel()
                    except Exception:  # noqa: BLE001
                        pass
                    conn = None
            # Release any lock / uncommitted state the OCI connection still
            # holds from this case *before* running teardown on the second
            # connection — otherwise a teardown `DROP` blocks on that lock.
            if conn is not None:
                try:
                    conn.rollback()
                except Exception:  # noqa: BLE001
                    pass
            for td in c.teardown:
                try:
                    psql(td, tolerant=True)
                except Exception:  # noqa: BLE001
                    pass
            if crashed or case_mutates(c):
                if conn is not None:
                    try:
                        conn.rollback()
                    except Exception:  # noqa: BLE001
                        pass
                if crashed:
                    if conn is not None:
                        try:
                            conn.close()
                        except Exception:  # noqa: BLE001
                            pass
                    # A reconnect can transiently fail (DbSaci mid-restart, a
                    # socket in TIME_WAIT). Retry before giving up on the run.
                    conn = None
                    for _ in range(20):
                        try:
                            conn = connect()
                            break
                        except Exception:  # noqa: BLE001
                            import time as _t
                            _t.sleep(1)
                    if conn is None:
                        print("FATAL: could not reconnect after 20s", flush=True)
                        conn = connect()  # raise the real error

            if ok:
                npass += 1
            else:
                nfail += 1
                failures.append((full, msg))
                print(f"FAIL {full}: {msg}", flush=True)
            done = npass + nfail
            if done % 50 == 0:
                print(f"... {done} run ({npass} pass / {nfail} fail)", flush=True)

    print()
    for name, msg in failures:
        print(f"FAIL {name}: {msg}")
    print(f"\n{npass} passed; {nfail} failed; {nskip} skipped")
    return min(nfail, 125)


_CREATE_RE = re.compile(
    r"^\s*CREATE\s+(?:GLOBAL\s+TEMPORARY\s+|OR\s+REPLACE\s+)?"
    r"(TABLE|VIEW|MATERIALIZED\s+VIEW|SEQUENCE)\s+([A-Za-z_][A-Za-z0-9_$]*)",
    re.IGNORECASE,
)


def run_case(conn, c: Case):
    # DDL implicitly commits (Oracle-correct), so `conn.rollback()` can't undo a
    # `-- setup: CREATE ...`. Drop any object a setup creates first, on the
    # direct PG connection, so a case is re-runnable.
    for setup in c.setup:
        m = _CREATE_RE.match(setup.lstrip("\0"))
        if m:
            kind = m.group(1).upper().replace("MATERIALIZED VIEW", "MATERIALIZED VIEW")
            try:
                psql(f"DROP {kind} IF EXISTS {m.group(2)} CASCADE", tolerant=True)
            except Exception:  # noqa: BLE001
                pass
    import time as _t
    _pace = float(os.environ.get("DBSACI_OCI_PACE_MS", "30")) / 1000.0
    for setup in c.setup:
        if _pace > 0:
            _t.sleep(_pace)
        if setup.startswith("\0"):
            try:
                cur = conn.cursor(); cur.execute(setup[1:]); cur.close()
            except Exception:  # noqa: BLE001
                pass
        else:
            cur = conn.cursor()
            try:
                cur.execute(setup)
            finally:
                cur.close()
    if _pace > 0:
        _t.sleep(_pace)
    binds = decode_binds(c.binds)

    if c.kind == "ok":
        cur = conn.cursor()
        try:
            cur.execute(c.sql, binds)
        finally:
            cur.close()
        return _verify(conn, c)

    if c.kind == "rows":
        try:
            rows = run_query_all(conn, c.sql, binds)
        except Exception as e:  # noqa: BLE001
            return False, f"expected rows, got error: {e}"
        actual = fmt_rows(rows)
        if actual != c.payload:
            return False, f"row mismatch\n  expected: {c.payload}\n  actual:   {actual}"
        return _verify(conn, c)

    if c.kind == "regex":
        try:
            rows = run_query_all(conn, c.sql, binds)
        except Exception as e:  # noqa: BLE001
            return False, f"expected a row, got error: {e}"
        actual = "\n".join(fmt_rows(rows))
        if not re.search(c.payload, actual):
            return False, f"row {actual!r} does not match regex {c.payload!r}"
        return _verify(conn, c)

    if c.kind == "rows_exactly":
        try:
            rows = run_query_all(conn, c.sql, binds)
        except Exception as e:  # noqa: BLE001
            return False, f"expected {c.payload} rows, got error: {e}"
        if len(rows) != c.payload:
            return False, f"expected exactly {c.payload} rows, got {len(rows)}"
        return _verify(conn, c)

    if c.kind == "rowcount":
        cur = conn.cursor()
        try:
            cur.execute(c.sql, binds)
            got = cur.rowcount
        except Exception as e:  # noqa: BLE001
            return False, f"expected {c.payload} rows affected, got error: {e}"
        finally:
            cur.close()
        if got != c.payload:
            return False, f"expected {c.payload} rows affected, got {got}"
        return _verify(conn, c)

    if c.kind == "error":
        want = c.payload
        forbid = None
        if " ~ " in want:
            want, forbid = (x.strip() for x in want.split(" ~ ", 1))
        try:
            run_query_all(conn, c.sql, binds)
            return False, f"expected error containing `{want}`, statement succeeded"
        except Exception as e:  # noqa: BLE001
            text = str(e)
            low = text.lower()
            if want.lower() not in low:
                return False, f"expected error containing `{want}`, got `{text}`"
            if forbid and forbid.lower() in low:
                return False, f"error should not contain `{forbid}`, got `{text}`"
        return _verify(conn, c)

    return False, f"unknown expectation kind {c.kind}"


def _verify(conn, c: Case):
    if not c.verify:
        return True, ""
    sql, expected = c.verify
    try:
        got = psql(sql)
    except Exception as e:  # noqa: BLE001
        return False, f"`-- verify` query failed: {e}"
    if got != expected:
        return False, f"independent connection sees `{got}`, expected `{expected}`"
    return True, ""


if __name__ == "__main__":
    _rc = main()
    # A timed-out case leaves a non-daemon worker thread stuck; `os._exit`
    # skips the interpreter's join-all-threads shutdown so the process ends.
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(_rc)
