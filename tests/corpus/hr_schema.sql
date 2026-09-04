# HR-style sample schema (regions / countries / departments / employees).
# Values authored to the Oracle-correct answer. Not a dump of Oracle's
# db-sample-schemas; a compact stand-in that exercises the same join/DML
# surface a vendor HR schema uses.

-- fixture: CREATE TABLE hr_regions (region_id INTEGER PRIMARY KEY, region_name VARCHAR(25) NOT NULL)
-- fixture: CREATE TABLE hr_countries (country_id CHAR(2) PRIMARY KEY, country_name VARCHAR(40), region_id INTEGER REFERENCES hr_regions(region_id))
-- fixture: CREATE TABLE hr_departments (department_id INTEGER PRIMARY KEY, department_name VARCHAR(30) NOT NULL, manager_id INTEGER)
-- fixture: CREATE TABLE hr_employees (employee_id INTEGER PRIMARY KEY, first_name VARCHAR(20), last_name VARCHAR(25) NOT NULL, email VARCHAR(25) NOT NULL, hire_date DATE NOT NULL, job_id VARCHAR(10) NOT NULL, salary NUMERIC(8,2), department_id INTEGER REFERENCES hr_departments(department_id))
-- fixture: INSERT INTO hr_regions VALUES (1, 'Europe'), (2, 'Americas')
-- fixture: INSERT INTO hr_countries VALUES ('DE', 'Germany', 1), ('US', 'United States of America', 2)
-- fixture: INSERT INTO hr_departments VALUES (10, 'Administration', 200), (50, 'Shipping', 121), (80, 'Sales', 145)
-- fixture: INSERT INTO hr_employees VALUES (100, 'Steven', 'King', 'SKING', '2003-06-17', 'AD_PRES', 24000, 10)
-- fixture: INSERT INTO hr_employees VALUES (101, 'Neena', 'Kochhar', 'NKOCHHAR', '2005-09-21', 'AD_VP', 17000, 10)
-- fixture: INSERT INTO hr_employees VALUES (121, 'Adam', 'Fripp', 'AFRIPP', '2005-04-10', 'ST_MAN', 8200, 50)
-- fixture: INSERT INTO hr_employees VALUES (145, 'John', 'Russell', 'JRUSSEL', '2004-10-01', 'SA_MAN', 14000, 80)

-- case: hr_employee_count
SELECT COUNT(*) FROM hr_employees
-- expect:
4
-- end

-- case: hr_president_salary
SELECT salary FROM hr_employees WHERE last_name = 'King'
-- expect:
24000
-- end

-- case: hr_join_department
SELECT e.last_name, d.department_name FROM hr_employees e JOIN hr_departments d ON d.department_id = e.department_id WHERE e.employee_id = 100
-- expect:
King | Administration
-- end

-- case: hr_nvl_commission_style
SELECT NVL(salary, 0) FROM hr_employees WHERE employee_id = 100
-- expect:
24000
-- end

-- case: hr_decode_job
SELECT DECODE(job_id, 'AD_PRES', 'President', 'Other') FROM hr_employees WHERE employee_id = 100
-- expect:
President
-- end

-- case: hr_insert_and_rollback
INSERT INTO hr_employees (employee_id, first_name, last_name, email, hire_date, job_id, salary, department_id) VALUES (200, 'Jennifer', 'Whalen', 'JWHALEN', DATE '2003-09-17', 'AD_ASST', 4400, 10)
-- rowcount: 1
-- end
