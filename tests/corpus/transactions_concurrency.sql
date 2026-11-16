# Oracle's transaction model, seen from two sessions.
#
# Oracle gives statement-level read consistency (READ COMMITTED): each
# statement sees what was committed before it started, even inside an open
# transaction. `-- concurrent:` runs on a second, autocommitting session.

-- case: committed_row_visible_to_next_statement
# A session that has read once (a transaction is open) still sees another
# session's later commit on its next statement — it must not be pinned to the
# snapshot of its first read.
-- setup: CREATE TABLE cc_vis (n NUMBER)
-- setup: SELECT COUNT(*) FROM cc_vis
-- concurrent: INSERT INTO cc_vis VALUES (1)
SELECT COUNT(*) FROM cc_vis
-- expect:
1
-- teardown: DROP TABLE IF EXISTS cc_vis
-- end

-- case: committed_row_visible_after_own_uncommitted_write
-- setup: CREATE TABLE cc_vis2 (n NUMBER)
-- setup: INSERT INTO cc_vis2 VALUES (1)
-- concurrent: INSERT INTO cc_vis2 VALUES (2)
SELECT COUNT(*) FROM cc_vis2
-- expect:
2
-- teardown: DROP TABLE IF EXISTS cc_vis2
-- end

-- case: serializable_transaction_reads_its_snapshot
# Oracle SERIALIZABLE is a transaction-level snapshot: later commits by other
# sessions stay invisible until this transaction ends.
-- setup: CREATE TABLE cc_ser (n NUMBER)
-- setup: SET TRANSACTION ISOLATION LEVEL SERIALIZABLE
-- setup: SELECT COUNT(*) FROM cc_ser
-- concurrent: INSERT INTO cc_ser VALUES (1)
SELECT COUNT(*) FROM cc_ser
-- expect:
0
-- teardown: DROP TABLE IF EXISTS cc_ser
-- end

-- case: set_transaction_after_dml_is_rejected
-- setup: CREATE TABLE cc_st (n NUMBER)
-- setup: INSERT INTO cc_st VALUES (1)
SET TRANSACTION READ ONLY
-- error: ORA-01453
-- teardown: DROP TABLE IF EXISTS cc_st
-- end

-- case: read_only_transaction_rejects_writes
-- setup: CREATE TABLE cc_ro (n NUMBER)
-- setup: SET TRANSACTION READ ONLY
INSERT INTO cc_ro VALUES (1)
-- error: ORA-
-- teardown: DROP TABLE IF EXISTS cc_ro
-- end

-- case: alter_session_survives_rollback
# ALTER SESSION is not transactional in Oracle.
-- setup: ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'
-- setup: ROLLBACK
SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_DATE_FORMAT'
-- expect:
YYYY-MM-DD
-- end
