# -*- coding: utf-8 -*-
import oracledb
oracledb.init_oracle_client(lib_dir=r"C:\Program Files\Oracle\instantclient_19_32")
c = oracledb.connect(user="system", password="oracle", dsn="127.0.0.1:15211/XEPDB1")
print("thin?", oracledb.is_thin_mode())

def t(label, sql, binds):
    cur = c.cursor()
    try:
        cur.execute(sql, binds)
        try:
            print(label, "-> OK rows:", cur.fetchall())
        except Exception:
            print(label, "-> OK rowcount:", cur.rowcount)
    except Exception as e:
        print(label, "-> ERR:", str(e).splitlines()[0])
    finally:
        cur.close()

# 1: placeholder inside string literal, 1 surplus bind supplied
t("placeholder_in_string_literal", "SELECT ':1' FROM DUAL", ["ignored"])
# 1b: same but positional list style
try:
    cur = c.cursor(); cur.execute("SELECT ':1' FROM DUAL", ["ignored"]); print("1b list ok", cur.fetchall()); cur.close()
except Exception as e:
    print("1b list ERR:", str(e).splitlines()[0])

# 2: surplus bind values (2 supplied, 1 referenced)
t("surplus_bind_values", "SELECT :1 FROM DUAL", [7, 99])

# 3: RETURNING with no INTO
try:
    cur = c.cursor(); cur.execute("CREATE TABLE v3_people (id NUMBER, name VARCHAR2(20))"); cur.close()
except Exception as e:
    print("create:", str(e).splitlines()[0])
cur = c.cursor(); cur.execute("INSERT INTO v3_people VALUES (2, 'x')"); c.commit(); cur.close()
t("update_returning_no_into", "UPDATE v3_people SET name = 'Hopper' WHERE id = :1 RETURNING name", [2])
try:
    cur = c.cursor(); cur.execute("DROP TABLE v3_people"); cur.close()
except Exception:
    pass
c.close()
