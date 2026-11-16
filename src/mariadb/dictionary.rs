//! The Oracle data dictionary and built-ins DbSaci provides on MariaDB.
//!
//! Everything DbSaci installs lives in one database, [`SCHEMA`], installed once
//! per server (versioned, under a lock) instead of being re-created in every
//! user's schema on every connect. MariaDB has no synonyms or `search_path`,
//! so the translator routes Oracle names to these objects itself:
//!
//! * a relation named in [`RELATIONS`] — unqualified, or qualified with `SYS`
//!   or `PUBLIC` — becomes `` `dbsaci_sys`.`<name>` `` (Oracle's dictionary views
//!   are public synonyms for `SYS` objects);
//! * a call to a function in [`FUNCTIONS`] becomes `` `dbsaci_sys`.`<name>`( ``;
//! * a call to `PACKAGE.MEMBER` listed in [`PACKAGE_ROUTINES`] becomes
//!   `` `dbsaci_sys`.`<routine>`( ``.
//!
//! The physical names here are fixed lower case and always back-quoted by the
//! translator, so they are independent of the identifier-case setting.

/// The database holding DbSaci's dictionary views and compatibility routines.
pub const SCHEMA: &str = "dbsaci_sys";

/// Oracle dictionary relations DbSaci provides, by their (lower-case) Oracle
/// name. `$` names (`v$version`) are included verbatim.
pub const RELATIONS: &[&str] = &[
    // USER_* / ALL_* catalog
    "all_tables",
    "user_tables",
    "all_all_tables",
    "all_object_tables",
    "all_tab_columns",
    "user_tab_columns",
    "all_tab_cols",
    "user_tab_cols",
    "all_objects",
    "user_objects",
    "all_constraints",
    "user_constraints",
    "all_cons_columns",
    "user_cons_columns",
    "all_indexes",
    "user_indexes",
    "all_ind_columns",
    "user_ind_columns",
    "all_ind_expressions",
    "all_sequences",
    "user_sequences",
    "all_tab_comments",
    "user_tab_comments",
    "all_col_comments",
    "user_col_comments",
    "all_triggers",
    "user_triggers",
    "all_trigger_cols",
    "all_views",
    "user_views",
    "all_users",
    "all_synonyms",
    "user_synonyms",
    "all_mviews",
    "user_mviews",
    "all_mview_logs",
    "all_mview_comments",
    "all_registered_mviews",
    "all_tab_identity_cols",
    "all_tab_privs",
    "all_col_privs",
    "all_role_privs",
    "all_tab_partitions",
    "all_tab_subpartitions",
    "all_part_tables",
    "all_part_key_columns",
    "all_ind_partitions",
    "all_lobs",
    "all_nested_tables",
    "all_external_tables",
    "all_clusters",
    "all_coll_types",
    "all_type_attrs",
    "all_directories",
    "all_editioning_views",
    "all_identifiers",
    "all_java_classes",
    "all_queue_tables",
    "all_scheduler_programs",
    "all_xml_schemas",
    "all_tab_col_statistics",
    "user_tablespaces",
    "user_dependencies",
    "session_privs",
    "session_roles",
    // instance / session
    "v$version",
    "product_component_version",
    "global_name",
    "nls_session_parameters",
    "nls_database_parameters",
];

/// Oracle built-in functions (and DbSaci helper functions the translator
/// emits) implemented as stored functions in [`SCHEMA`], by lower-case name.
pub const FUNCTIONS: &[&str] = &[
    "sys_context",
    "sessiontimezone",
    "dbtimezone",
    "months_between",
    "next_day",
    "initcap",
    "numtodsinterval",
    "numtoyminterval",
    "regexp_count",
    "oracle_instr",
    "oracle_regexp_substr",
    "oracle_translate",
    "dbsaci_div",
    "dbsaci_num_add",
    "dbsaci_to_date",
    "dbsaci_to_number",
    "listagg",
];

/// `(package, member, routine)`: Oracle package members implemented as a
/// stored routine named `routine` in [`SCHEMA`].
pub const PACKAGE_ROUTINES: &[(&str, &str, &str)] = &[
    ("dbms_output", "put_line", "dbms_output__put_line"),
    ("dbms_output", "put", "dbms_output__put"),
    ("dbms_output", "new_line", "dbms_output__new_line"),
    ("dbms_output", "enable", "dbms_output__enable"),
    ("dbms_output", "disable", "dbms_output__disable"),
    ("dbms_metadata", "get_ddl", "dbms_metadata__get_ddl"),
];

/// Is `name` (any case) one of the dictionary relations?
pub fn is_relation(name: &str) -> bool {
    RELATIONS.iter().any(|r| r.eq_ignore_ascii_case(name))
}

/// Is `name` (any case) one of the compatibility functions?
pub fn is_function(name: &str) -> bool {
    FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(name))
}

/// The routine implementing `package.member`, if DbSaci provides one.
pub fn package_routine(package: &str, member: &str) -> Option<&'static str> {
    PACKAGE_ROUTINES
        .iter()
        .find(|(p, m, _)| p.eq_ignore_ascii_case(package) && m.eq_ignore_ascii_case(member))
        .map(|(_, _, r)| *r)
}
