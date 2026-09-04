# Oracle-focused capability cases provided directly by MariaDB's ORACLE mode.
# They are not MariaDB SQL feature tests. PostgreSQL's current PL/SQL lowering
# does not implement Oracle packages, so this group documents a real MariaDB
# compatibility advantage while keeping the shared corpus Oracle-shaped.
# skip: postgres (PostgreSQL backend has no Oracle PACKAGE lowering)

-- case: oracle_package_function
-- setup: CREATE OR REPLACE PACKAGE dbsaci_pkg AS FUNCTION answer RETURN NUMBER; END;
-- setup: CREATE OR REPLACE PACKAGE BODY dbsaci_pkg AS FUNCTION answer RETURN NUMBER AS BEGIN RETURN 42; END; END;
SELECT dbsaci_pkg.answer() FROM DUAL
-- expect:
42
-- end
