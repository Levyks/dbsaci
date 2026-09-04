//! Per-user PostgreSQL credentials.
//!
//! An Oracle client authenticates with a challenge/response: the password never
//! crosses the wire, only a proof derived from it. DbSaci therefore has to
//! *already know* the password so it can (a) run the same derivation and verify
//! the client's proof, and (b) open the backend PostgreSQL connection with it.
//!
//! The model is a pre-declared list of `pg_user -> pg_password` pairs (from a
//! file, `DBSACI_PG_USERS`, and/or repeated `--pg-user` flags). On login DbSaci
//! looks the Oracle username up in that list (case-insensitively — PostgreSQL
//! folds unquoted role names to lower case) and runs the challenge with the
//! matching password, so an Oracle client logs in with the *same* credentials
//! it would use for PostgreSQL directly. A user absent from the list falls back
//! to the single `pg_password` / `DBSACI_PG_PASSWORD` if one is set, and is
//! rejected with ORA-01017 otherwise.

use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    users: HashMap<String, String>,
    /// Applied to any user not named in `users`. `None` => unknown users are
    /// rejected.
    fallback: Option<String>,
}

impl Credentials {
    /// A store where every user authenticates with `password` (the pre-list
    /// behaviour). Used by the corpus harness and as `Config`'s default.
    pub fn with_fallback(password: impl Into<String>) -> Self {
        Self {
            users: HashMap::new(),
            fallback: Some(password.into()),
        }
    }

    pub fn set_fallback(&mut self, password: Option<String>) {
        self.fallback = password.filter(|p| !p.is_empty());
    }

    /// Register one `user -> password` pair. Later calls override earlier ones,
    /// so callers can layer file < env < CLI.
    pub fn insert(&mut self, user: impl AsRef<str>, password: impl Into<String>) {
        self.users
            .insert(user.as_ref().trim().to_ascii_lowercase(), password.into());
    }

    /// Parse `user:password` items (one per element) into the store.
    /// `user:pass:word` keeps everything after the first colon as the password.
    pub fn extend_pairs<I, S>(&mut self, items: I) -> Result<(), String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for item in items {
            let item = item.as_ref().trim();
            if item.is_empty() || item.starts_with('#') {
                continue;
            }
            let (user, password) = item
                .split_once(':')
                .ok_or_else(|| format!("credential {item:?} is not `user:password`"))?;
            if user.trim().is_empty() {
                return Err(format!("credential {item:?} has an empty user"));
            }
            self.insert(user, password);
        }
        Ok(())
    }

    /// Parse a comma-separated `user:pass,user:pass` string.
    pub fn extend_comma_list(&mut self, list: &str) -> Result<(), String> {
        self.extend_pairs(list.split(','))
    }

    /// Parse a file of `user:password` lines (`#` comments, blank lines ignored).
    pub fn extend_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        self.extend_pairs(text.lines())
    }

    /// The password to run the login challenge / backend connection with, or
    /// `None` to reject the login.
    pub fn password_for(&self, user: &str) -> Option<&str> {
        self.users
            .get(&user.trim().to_ascii_lowercase())
            .map(String::as_str)
            .or(self.fallback.as_deref())
    }

    pub fn named_user_count(&self) -> usize {
        self.users.len()
    }

    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive_and_layered() {
        let mut c = Credentials::default();
        c.extend_file(std::path::Path::new("/nonexistent")).ok(); // tolerated by caller
        c.extend_comma_list("alice:a1, BOB:b1").unwrap();
        c.insert("alice", "a2"); // CLI overrides env
        assert_eq!(c.password_for("ALICE"), Some("a2"));
        assert_eq!(c.password_for("bob"), Some("b1"));
        assert_eq!(c.password_for("carol"), None);
        c.set_fallback(Some("wild".into()));
        assert_eq!(c.password_for("carol"), Some("wild"));
        c.set_fallback(Some(String::new()));
        assert_eq!(c.password_for("carol"), None);
    }

    #[test]
    fn rejects_malformed_pair() {
        let mut c = Credentials::default();
        assert!(c.extend_comma_list("noseparator").is_err());
        assert!(c.extend_comma_list(":emptyuser").is_err());
        c.extend_comma_list("u:p:w").unwrap();
        assert_eq!(c.password_for("u"), Some("p:w"));
    }
}
