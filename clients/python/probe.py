"""python-oracledb (thin mode) compatibility probe against a running DbSaci.

Env: DBSACI_HOST, DBSACI_PORT, DBSACI_USER, DBSACI_PASSWORD, DBSACI_SERVICE.
Exits non-zero on the first failed assertion.
"""
import os
import sys
import oracledb

host = os.environ.get("DBSACI_HOST", "127.0.0.1")
port = int(os.environ.get("DBSACI_PORT", "1521"))
user = os.environ.get("DBSACI_USER", "corpus")
password = os.environ.get("DBSACI_PASSWORD", "corpus")
service = os.environ.get("DBSACI_SERVICE", "FREEPDB1")

dsn = oracledb.makedsn(host, port, service_name=service)
print(f"connecting thin mode to {dsn} as {user}")
conn = oracledb.connect(user=user, password=password, dsn=dsn)
print("connected; server version:", conn.version)

checks = []

def check(name, got, want):
    ok = got == want
    checks.append((name, ok, got, want))
    print(("PASS " if ok else "FAIL "), name, "->", repr(got), "" if ok else f"(want {want!r})")

cur = conn.cursor()

# 1. trivial SELECT
cur.execute("SELECT 1 FROM DUAL")
check("select_1_from_dual", cur.fetchone(), (1,))

# 2. SELECT over seeded data
cur.execute("SELECT name FROM people WHERE id = 1")
check("select_people_row", cur.fetchone(), ("Ada",))

# 3. bind parameters (named)
cur.execute("SELECT name FROM people WHERE id = :pid", pid=3)
check("named_bind", cur.fetchone(), ("Linus",))

# 4. bind parameters (positional)
cur.execute("SELECT COUNT(*) FROM people WHERE team_id = :1", [1])
check("positional_bind", cur.fetchone(), (2,))

# 5. DML + rowcount, then rollback
cur.execute("INSERT INTO people (id, name, team_id) VALUES (:1, :2, :3)", [90, "Zed", 2])
check("insert_rowcount", cur.rowcount, 1)
cur.execute("SELECT name FROM people WHERE id = 90")
check("insert_visible_same_txn", cur.fetchone(), ("Zed",))
conn.rollback()
cur.execute("SELECT COUNT(*) FROM people WHERE id = 90")
check("rollback_removed_row", cur.fetchone(), (0,))

# 6. larger result set (exercise the fetch loop)
cur.arraysize = 100
cur.execute("SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 2500 ORDER BY LEVEL")
rows = cur.fetchall()
check("large_result_count", len(rows), 2500)
check("large_result_first", rows[0], (1,))
check("large_result_last", rows[-1], (2500,))

# 7. Oracle-ism translated on the way through
cur.execute("SELECT NVL(TO_CHAR(team_id), 'none') FROM people WHERE id = 4")
check("nvl_translation", cur.fetchone(), ("none",))

# 8. anonymous PL/SQL block
cur.execute("BEGIN NULL; END;")
check("plsql_block_runs", True, True)

# 8b. native TIMESTAMP result column keeps sub-second precision
cur.execute("SELECT CAST(TIMESTAMP '2024-02-29 13:14:15.123456' AS TIMESTAMP) FROM DUAL")
ts = cur.fetchone()[0]
check("timestamp_microseconds", getattr(ts, "microsecond", None), 123456)

# 8c. declared NUMBER(p,s) is reported with its real precision/scale
cur.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE numps_demo'; EXCEPTION WHEN OTHERS THEN NULL; END;")
cur.execute("CREATE TABLE numps_demo (price NUMBER(10,2))")
cur.execute("INSERT INTO numps_demo (price) VALUES (1234.56)")
cur.execute("SELECT price FROM numps_demo")
d = cur.description[0]
check("number_ps_precision", d[4], 10)
check("number_ps_scale", d[5], 2)
check("number_ps_value", cur.fetchone()[0], 1234.56)
cur.execute("DROP TABLE numps_demo")

# 8d-i. INTERVAL result columns decode natively (types 182 / 183)
import datetime as _dt
cur.execute("SELECT CAST('9 06:30:00' AS INTERVAL DAY TO SECOND) FROM DUAL")
check("interval_ds_value", cur.fetchone()[0], _dt.timedelta(days=9, hours=6, minutes=30))
cur.execute("SELECT NUMTODSINTERVAL(-2.5, 'DAY') FROM DUAL")
check("interval_ds_negative", cur.fetchone()[0], _dt.timedelta(days=-2, hours=-12))
cur.execute("SELECT CAST('1-6' AS INTERVAL YEAR TO MONTH) FROM DUAL")
ym = cur.fetchone()[0]
check("interval_ym_value", (ym.years, ym.months), (1, 6))

# 8d. declared BINARY_DOUBLE column comes back as a native float
cur.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE bd_demo'; EXCEPTION WHEN OTHERS THEN NULL; END;")
cur.execute("CREATE TABLE bd_demo (v BINARY_DOUBLE)")
cur.execute("INSERT INTO bd_demo (v) VALUES (3.5)")
cur.execute("SELECT v FROM bd_demo")
check("binary_double_value", cur.fetchone()[0], 3.5)
check("binary_double_type", cur.description[0][1], oracledb.DB_TYPE_BINARY_DOUBLE)
cur.execute("DROP TABLE bd_demo")

# 8e. PostgreSQL statement error position is surfaced in the ORA error offset
try:
    cur.execute("SELECT 1 FROM people WHERE nonexistent_col_xyz = 1")
    check("error_position_raised", False, True)
except oracledb.DatabaseError as e:
    (err,) = e.args
    check("error_position_nonzero", err.offset > 0, True)

# 8f. RETURNING ... INTO OUT bind
rid = cur.var(int)
cur.execute(
    "INSERT INTO people (id, name, team_id) VALUES (:1, :2, :3) RETURNING id INTO :rid",
    [77, "Ret", 1, rid],
)
check("returning_into_value", rid.getvalue(), [77])
check("returning_into_rowcount", cur.rowcount, 1)
cur.execute("SELECT name FROM people WHERE id = 77")
check("returning_into_visible", cur.fetchone(), ("Ret",))
rid2 = cur.var(int)
cur.execute("UPDATE people SET team_id = 3 WHERE id = 77 RETURNING team_id INTO :r", [rid2])
check("returning_into_update", rid2.getvalue(), [3])
conn.rollback()

# 9. array binds (executemany / batch DML)
cur.executemany(
    "INSERT INTO people (id, name, team_id) VALUES (:1, :2, :3)",
    [(101, "Ann", 1), (102, "Bo", 2), (103, "Cy", 3), (104, "Di", 1)],
)
check("executemany_rowcount", cur.rowcount, 4)
cur.execute("SELECT COUNT(*) FROM people WHERE id BETWEEN 101 AND 104")
check("executemany_visible", cur.fetchone(), (4,))
cur.executemany("UPDATE people SET team_id = :2 WHERE id = :1", [(101, 2), (102, 2)])
check("executemany_update_rowcount", cur.rowcount, 2)
conn.rollback()
cur.execute("SELECT COUNT(*) FROM people WHERE id BETWEEN 101 AND 104")
check("executemany_rolled_back", cur.fetchone(), (0,))

# 10. re-execute of a no-bind, multi-batch query after bind traffic on the same
# cursor. python-oracledb thin re-runs a hot statement with a bare REEXECUTE
# (no SQL, no bind row). DbSaci must resolve that to the statement currently on
# the cursor. A regression resolved it via the wrapping TTC sequence byte, so
# after a big fetch (many round trips advance the seq) a later REEXECUTE's seq
# aliased a slot left by the earlier bind INSERT; DbSaci then expected a bind
# RowData marker the query re-execute never sends -> ORA-01008
# "reexecute: no bind RowData marker". Seen first by bench `big_fetch_25k_rows`.
cur.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE reexec_demo'; EXCEPTION WHEN OTHERS THEN NULL; END;")
cur.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE reexec_seed'; EXCEPTION WHEN OTHERS THEN NULL; END;")
cur.execute("CREATE TABLE reexec_seed (k NUMBER PRIMARY KEY)")
cur.execute("CREATE TABLE reexec_demo (id NUMBER PRIMARY KEY, n NUMBER, label VARCHAR2(40))")
for _k in range(1, 181):  # bind-carrying INSERT, re-executed -> seeds the seq map
    cur.execute("INSERT INTO reexec_seed (k) VALUES (:1)", [_k])
conn.commit()
cur.execute(
    "INSERT INTO reexec_demo (id, n, label) "
    "SELECT ((a.k - 1) * 180 + b.k), MOD((a.k - 1) * 180 + b.k, 100), "
    "       'lbl-' || ((a.k - 1) * 180 + b.k) "
    "FROM reexec_seed a, reexec_seed b "
    "WHERE ((a.k - 1) * 180 + b.k) <= 20000"
)
conn.commit()
cur.arraysize = 100
_reexec_ok = True
_reexec_err = None
for _it in range(6):
    try:
        cur.execute("SELECT id, n, label FROM reexec_demo WHERE id <= 20000")
        if len(cur.fetchall()) != 20000:
            _reexec_ok = False
            _reexec_err = "row count mismatch"
            break
    except oracledb.DatabaseError as _e:
        _reexec_ok = False
        _reexec_err = str(_e).splitlines()[0]
        break
check("nobind_query_reexecute_after_bind", _reexec_ok, True)
if not _reexec_ok:
    print("   detail:", _reexec_err)
cur.execute("DROP TABLE reexec_demo")
cur.execute("DROP TABLE reexec_seed")

cur.close()
conn.close()

failed = [c for c in checks if not c[1]]
print(f"\n{len(checks) - len(failed)}/{len(checks)} checks passed")
sys.exit(1 if failed else 0)
