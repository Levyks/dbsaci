# Explicit unimplemented Oracle features must fail with a clear ORA, never a
# wrong success. Keep this group small: it locks the 0.2.0 contract.

-- case: sys_refcursor_is_ora_3001
CREATE PROCEDURE open_emp (c OUT SYS_REFCURSOR) AS BEGIN OPEN c FOR SELECT 1 FROM dual; END;
-- error: ORA-03001
-- end

-- case: pipelined_function_is_ora_3001
CREATE FUNCTION pipe_ids RETURN sys.odcinumberlist PIPELINED AS BEGIN NULL; END;
-- error: ORA-03001
-- end
