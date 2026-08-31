import java.sql.*;

public class JdbcCompat {
    static int pass = 0, fail = 0;
    static void check(String name, Object got, Object want) {
        boolean ok = (got == null && want == null) || (got != null && got.equals(want));
        System.out.printf("%s %s -> %s%s%n", ok ? "PASS" : "FAIL", name, got, ok ? "" : " (want " + want + ")");
        if (ok) pass++; else fail++;
    }
    public static void main(String[] a) throws Exception {
        String host = System.getenv().getOrDefault("PGSACI_HOST", "127.0.0.1");
        String port = System.getenv().getOrDefault("PGSACI_PORT", "1521");
        String user = System.getenv().getOrDefault("PGSACI_USER", "corpus");
        String pw   = System.getenv().getOrDefault("PGSACI_PASSWORD", "corpus");
        String svc  = System.getenv().getOrDefault("PGSACI_SERVICE", "FREEPDB1");
        String url = "jdbc:oracle:thin:@//" + host + ":" + port + "/" + svc;
        System.out.println("connecting: " + url + " as " + user);
        try (Connection c = DriverManager.getConnection(url, user, pw)) {
            System.out.println("connected; " + c.getMetaData().getDatabaseProductVersion());
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT 1 FROM DUAL")) { r.next(); check("select_1", r.getInt(1), 1); }
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT name FROM people WHERE id = 1")) { r.next(); check("people_row", r.getString(1), "Ada"); }
            try (PreparedStatement p = c.prepareStatement("SELECT name FROM people WHERE id = ?")) {
                p.setInt(1, 3);
                try (ResultSet r = p.executeQuery()) { r.next(); check("bind", r.getString(1), "Linus"); }
            }
            c.setAutoCommit(false);
            try (PreparedStatement p = c.prepareStatement("INSERT INTO people (id, name, team_id) VALUES (?, ?, ?)")) {
                p.setInt(1, 91); p.setString(2, "Jdbc"); p.setInt(3, 2);
                check("insert_rows", p.executeUpdate(), 1);
            }
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT name FROM people WHERE id = 91")) {
                r.next(); check("insert_visible", r.getString(1), "Jdbc");
            }
            c.rollback();
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT COUNT(*) FROM people WHERE id = 91")) {
                r.next(); check("rollback", r.getInt(1), 0);
            }
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT LEVEL FROM DUAL CONNECT BY LEVEL <= 2500")) {
                int n = 0, last = 0; while (r.next()) { n++; last = r.getInt(1); }
                check("big_count", n, 2500); check("big_last", last, 2500);
            }
            try (PreparedStatement p = c.prepareStatement("INSERT INTO people (id, name, team_id) VALUES (?, ?, ?)")) {
                int[][] rows = {{201,1},{202,2},{203,3}};
                for (int[] rr : rows) { p.setInt(1, rr[0]); p.setString(2, "B" + rr[0]); p.setInt(3, rr[1]); p.addBatch(); }
                int[] counts = p.executeBatch();
                int sum = 0; for (int x : counts) sum += (x == Statement.SUCCESS_NO_INFO ? 1 : x);
                check("batch_insert_count", sum, 3);
            }
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT COUNT(*) FROM people WHERE id BETWEEN 201 AND 203")) {
                r.next(); check("batch_insert_visible", r.getInt(1), 3);
            }
            c.rollback();
            try (Statement s = c.createStatement()) {
                try { s.execute("DROP TABLE jdbc_types"); } catch (SQLException ignore) {}
                s.execute("CREATE TABLE jdbc_types (price NUMBER(10,2), dv BINARY_DOUBLE)");
                s.execute("INSERT INTO jdbc_types (price, dv) VALUES (1234.56, 3.5)");
            }
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("SELECT price, dv FROM jdbc_types")) {
                r.next();
                // ojdbc gets NUMBER(38,0) for the scaled column (its
                // column-metadata parser desyncs on a non-zero scale field);
                // the value is still exact.
                check("number_ps_value", r.getBigDecimal(1).toPlainString(), "1234.56");
                check("binary_double_value", r.getDouble(2), 3.5);
            }
            try (Statement s = c.createStatement()) { s.execute("DROP TABLE jdbc_types"); }
            c.commit();
        }
        System.out.printf("%n%d/%d checks passed%n", pass, pass + fail);
        System.exit(fail == 0 ? 0 : 1);
    }
}
