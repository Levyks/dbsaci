// ADO.NET client-compatibility probe against a running DbSaci proxy, mirroring
// the python-oracledb and JDBC probes. Uses Oracle.ManagedDataAccess.Core
// (Oracle's pure-managed thin provider).
//
// Env: DBSACI_HOST, DBSACI_PORT, DBSACI_USER, DBSACI_PASSWORD, DBSACI_SERVICE.
// Exits non-zero if any check fails.

using System.Data;
using Oracle.ManagedDataAccess.Client;

static string Env(string k, string dflt) =>
    Environment.GetEnvironmentVariable(k) is { Length: > 0 } v ? v : dflt;

string host = Env("DBSACI_HOST", "127.0.0.1");
string port = Env("DBSACI_PORT", "1521");
string user = Env("DBSACI_USER", "corpus");
string pw = Env("DBSACI_PASSWORD", "corpus");
string svc = Env("DBSACI_SERVICE", "FREEPDB1");

string dataSource =
    $"(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={host})(PORT={port}))" +
    $"(CONNECT_DATA=(SERVICE_NAME={svc})))";
// Pooling=false so conn.Close() issues a real LOGOFF instead of parking the
// socket in the pool (which would RST at process exit and noise up the log).
string connStr = $"User Id={user};Password={pw};Data Source={dataSource};Pooling=false;";

int pass = 0, fail = 0;
void Check(string name, object? got, object? want)
{
    bool ok = Equals(got?.ToString(), want?.ToString());
    Console.WriteLine($"{(ok ? "PASS" : "FAIL")} {name} -> {got}{(ok ? "" : $" (want {want})")}");
    if (ok) pass++; else fail++;
}

Console.WriteLine($"connecting: {dataSource} as {user}");
using var conn = new OracleConnection(connStr);
conn.Open();
Console.WriteLine($"connected; server version: {conn.ServerVersion}");

// 1. trivial SELECT
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT 1 FROM DUAL";
    Check("select_1", Convert.ToInt32(c.ExecuteScalar()), 1);
}

// 2. SELECT over seeded data
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT name FROM people WHERE id = 1";
    Check("people_row", c.ExecuteScalar(), "Ada");
}

// 3. named bind
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT name FROM people WHERE id = :pid";
    c.Parameters.Add(new OracleParameter("pid", 3));
    Check("bind", c.ExecuteScalar(), "Linus");
}

// 4. DML + rowcount, same-txn visibility, rollback
using (var tx = conn.BeginTransaction())
{
    using (var c = conn.CreateCommand())
    {
        c.Transaction = tx;
        c.CommandText = "INSERT INTO people (id, name, team_id) VALUES (:1, :2, :3)";
        c.Parameters.Add(new OracleParameter("1", 92));
        c.Parameters.Add(new OracleParameter("2", "Dot"));
        c.Parameters.Add(new OracleParameter("3", 2));
        Check("insert_rows", c.ExecuteNonQuery(), 1);
    }
    using (var c = conn.CreateCommand())
    {
        c.Transaction = tx;
        c.CommandText = "SELECT name FROM people WHERE id = 92";
        Check("insert_visible", c.ExecuteScalar(), "Dot");
    }
    tx.Rollback();
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT COUNT(*) FROM people WHERE id = 92";
    Check("rollback", Convert.ToInt32(c.ExecuteScalar()), 0);
}

// 5. larger result set (fetch loop) + an Oracle-ism translated on the way through
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 2500 ORDER BY LEVEL";
    c.FetchSize = 100 * 6;
    int n = 0, last = 0;
    using var r = c.ExecuteReader();
    while (r.Read()) { n++; last = Convert.ToInt32(r.GetValue(0)); }
    Check("big_count", n, 2500);
    Check("big_last", last, 2500);
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT NVL(TO_CHAR(team_id), 'none') FROM people WHERE id = 4";
    Check("nvl_translation", c.ExecuteScalar(), "none");
}

// 5b. declared NUMBER(p,s) + BINARY_DOUBLE column types
using (var c = conn.CreateCommand())
{
    c.CommandText = @"BEGIN EXECUTE IMMEDIATE 'DROP TABLE dn_types'; EXCEPTION WHEN OTHERS THEN NULL; END;";
    c.ExecuteNonQuery();
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "CREATE TABLE dn_types (price NUMBER(10,2), dv BINARY_DOUBLE)";
    c.ExecuteNonQuery();
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "INSERT INTO dn_types (price, dv) VALUES (1234.56, 3.5)";
    c.ExecuteNonQuery();
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT price, dv FROM dn_types";
    using var r = c.ExecuteReader();
    r.Read();
    Check("number_ps_value", r.GetDecimal(0), 1234.56m);
    Check("binary_double_value", r.GetDouble(1), 3.5);
    var schema = r.GetSchemaTable();
    if (schema != null)
    {
        Check("number_ps_precision", schema.Rows[0]["NumericPrecision"], 10);
        Check("number_ps_scale", schema.Rows[0]["NumericScale"], 2);
    }
}
using (var c = conn.CreateCommand())
{
    c.CommandText = "DROP TABLE dn_types";
    c.ExecuteNonQuery();
}

using (var c = conn.CreateCommand())
{
    c.CommandText = "SELECT CAST(TIMESTAMP '2024-02-29 13:14:15' AS TIMESTAMP) FROM DUAL";
    using var r = c.ExecuteReader();
    r.Read();
    var tn = r.GetDataTypeName(0);
    Check("timestamp_type_name_is_timestamp", tn.ToUpperInvariant().Contains("TIMESTAMP"), true);
}

using (var c1 = conn.CreateCommand())
using (var c2 = conn.CreateCommand())
{
    c1.CommandText = "SELECT name FROM people WHERE id = 1";
    c2.CommandText = "SELECT name FROM people WHERE id = 2";
    using var r1 = c1.ExecuteReader();
    using var r2 = c2.ExecuteReader();
    r1.Read(); r2.Read();
    Check("multi_cursor_first", r1.GetString(0), "Ada");
    Check("multi_cursor_second", r2.GetString(0), "Grace");
}

// 6. array bind (ODP.NET ArrayBindCount / batch DML)
using (var tx = conn.BeginTransaction())
{
    using (var c = conn.CreateCommand())
    {
        c.Transaction = tx;
        c.CommandText = "INSERT INTO people (id, name, team_id) VALUES (:1, :2, :3)";
        c.ArrayBindCount = 3;
        c.Parameters.Add(new OracleParameter("1", OracleDbType.Int32) { Value = new int[] { 301, 302, 303 } });
        c.Parameters.Add(new OracleParameter("2", OracleDbType.Varchar2) { Value = new string[] { "N1", "N2", "N3" } });
        c.Parameters.Add(new OracleParameter("3", OracleDbType.Int32) { Value = new int[] { 1, 2, 3 } });
        Check("array_bind_count", c.ExecuteNonQuery(), 3);
    }
    using (var c = conn.CreateCommand())
    {
        c.Transaction = tx;
        c.CommandText = "SELECT COUNT(*) FROM people WHERE id BETWEEN 301 AND 303";
        Check("array_bind_visible", Convert.ToInt32(c.ExecuteScalar()), 3);
    }
    tx.Rollback();
}

conn.Close();
Console.WriteLine($"\n{pass}/{pass + fail} checks passed");
Environment.Exit(fail == 0 ? 0 : 1);
