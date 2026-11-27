//! Session start (`InitPostgres`, `process_startup_options`, `process_settings`): the
//! settings of `pg_db_role_setting` (ALTER ROLE / ALTER DATABASE ... SET) and those the
//! client sends in the startup packet, as `options` switches (`-c name=value`,
//! `--name=value`) or as plain parameters, become the session's defaults.

use super::{Session, SessionDefaults, isolation_of};
use pgmem_types::{SqlError, SqlResult, code};
use std::sync::Arc;

/// Startup packet parameters that are not settings.
fn is_connection_parameter(name: &str) -> bool {
    matches!(name, "user" | "database" | "replication" | "options") || name.starts_with("_pq_.")
}

/// `pg_split_opts`: the `options` string split at unescaped whitespace; a backslash makes the
/// next character literal.
pub fn split_options(options: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = options.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return out;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else {
                word.push(c);
            }
        }
        out.push(word);
    }
}

fn syntax_error(message: String) -> SqlError {
    SqlError::new(code::SYNTAX_ERROR, message)
}

/// `ParseLongOption`: `name=value` with dashes in the name read as underscores.
fn long_option(arg: &str, switch: &str) -> SqlResult<(String, String)> {
    match arg.split_once('=') {
        Some((name, value)) => Ok((name.replace('-', "_"), value.to_owned())),
        None => Err(syntax_error(format!("{switch}{arg} requires a value"))),
    }
}

/// The settings an `options` string sets, in order (`process_postgres_switches` for `-c` and
/// `--`; other switches are not supported).
pub fn parse_options(options: &str) -> SqlResult<Vec<(String, String)>> {
    let words = split_options(options);
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        if let Some(rest) = w.strip_prefix("--") {
            out.push(long_option(rest, "--")?);
        } else if let Some(rest) = w.strip_prefix("-c") {
            let arg = if rest.is_empty() {
                i += 1;
                words.get(i).ok_or_else(|| invalid_argument(w))?
            } else {
                rest
            };
            out.push(long_option(arg, "-c ")?);
        } else {
            return Err(invalid_argument(w));
        }
        i += 1;
    }
    Ok(out)
}

fn invalid_argument(arg: &str) -> SqlError {
    syntax_error(format!("invalid command-line argument for server process: {arg}"))
        .with_hint("Try \"postgres --help\" for more information.")
}

/// Validates a setting given at session start (`set_config_option` in `PGC_BACKEND` /
/// `PGC_SU_BACKEND` context): backend parameters may be set now, server-level ones may not.
fn validate_at_start(name: &str, raw: &str, current: Option<&str>) -> SqlResult<String> {
    // Reported but not in pg_settings (GUC_NO_SHOW_ALL), and internal.
    if name == "is_superuser" {
        return Err(SqlError::new(code::CANT_CHANGE_RUNTIME_PARAM, "parameter \"is_superuser\" cannot be changed"));
    }
    if name == "search_path" && !is_identifier_list(raw) {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"search_path\": \"{raw}\""))
            .with_detail("List syntax is invalid."));
    }
    if let Some(def) = crate::settings::lookup(name) {
        let msg = match def.context {
            "internal" => Some(format!("parameter \"{}\" cannot be changed", def.name)),
            "postmaster" => Some(format!("parameter \"{}\" cannot be changed without restarting the server", def.name)),
            "sighup" => Some(format!("parameter \"{}\" cannot be changed now", def.name)),
            _ => None,
        };
        if let Some(msg) = msg {
            return Err(SqlError::new(code::CANT_CHANGE_RUNTIME_PARAM, msg));
        }
        if matches!(def.context, "backend" | "superuser-backend") {
            return crate::settings::normalize(def, raw, current);
        }
    }
    super::validate_setting(name, raw, current)
}

/// `SplitIdentifierString`'s syntax check: comma-separated names, each double-quoted (`""`
/// escapes a quote) or a run of characters without whitespace or commas, with optional
/// whitespace around them. An empty string is an empty list.
fn is_identifier_list(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    let skip_ws = |c: &mut std::iter::Peekable<std::str::Chars>| while c.next_if(char::is_ascii_whitespace).is_some() {};
    skip_ws(&mut chars);
    if chars.peek().is_none() {
        return true;
    }
    loop {
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next() {
                    None => return false,
                    Some('"') if chars.next_if_eq(&'"').is_none() => break,
                    Some(_) => {}
                }
            }
        } else {
            let mut empty = true;
            while chars.next_if(|c| !c.is_ascii_whitespace() && *c != ',').is_some() {
                empty = false;
            }
            if empty {
                return false;
            }
        }
        skip_ws(&mut chars);
        match chars.next() {
            None => return true,
            Some(',') => skip_ws(&mut chars),
            Some(_) => return false,
        }
    }
}

impl Session {
    /// Applies the settings a new session starts with, lowest priority first:
    /// `ALTER ROLE ALL SET` (source `global`), `ALTER DATABASE SET` (`database`),
    /// `ALTER ROLE SET` (`user`), `ALTER ROLE ... IN DATABASE SET` (`database user`), then
    /// the startup packet's `options` and its other parameters (`client`). They become the
    /// session defaults RESET restores. An invalid client setting is an error (the connection
    /// fails); an invalid stored one is skipped.
    pub fn apply_startup(&mut self, params: &[(String, String)]) -> SqlResult<()> {
        let mut client = Vec::new();
        if let Some((_, options)) = params.iter().find(|(k, _)| k == "options") {
            client.extend(parse_options(options)?);
        }
        for (k, v) in params {
            let key = k.to_ascii_lowercase();
            // The session time zone and encoding are not taken from plain startup parameters
            // (drivers like pgjdbc send the JVM's zone).
            if !is_connection_parameter(&key) && key != "timezone" && key != "client_encoding" {
                client.push((key, v.clone()));
            }
        }
        let catalog = self.catalog_snapshot();
        let role = crate::roles::setting_role_oid(&catalog, &self.user);
        let database = self.database.clone();
        let mut defaults = SessionDefaults::new();
        let stored =
            [("", 0, "global"), (database.as_str(), 0, "database"), ("", role, "user"), (database.as_str(), role, "database user")];
        for (db, role, source) in stored {
            for entry in catalog.db_role_setting(db, role) {
                let Some((name, value)) = entry.split_once('=') else { continue };
                let key = name.to_ascii_lowercase();
                // SET ROLE / SESSION AUTHORIZATION at session start are not applied.
                if matches!(key.as_str(), "role" | "session_authorization" | "transaction_isolation") {
                    continue;
                }
                let current = defaults.get(&key).map(|(v, _)| v.clone()).or_else(|| self.setting_value(&key));
                if let Ok(v) = validate_at_start(&key, value, current.as_deref()) {
                    defaults.insert(key, (v, source));
                }
            }
        }
        for (name, value) in client {
            let key = name.to_ascii_lowercase();
            let current = defaults.get(&key).map(|(v, _)| v.clone()).or_else(|| self.setting_value(&key));
            let v = validate_at_start(&key, &value, current.as_deref())?;
            // transaction_isolation is reset at each transaction start.
            if key != "transaction_isolation" {
                defaults.insert(key, (v, "client"));
            }
        }
        for (k, (v, _)) in &defaults {
            self.settings.insert(k.clone(), v.clone());
        }
        if let Some((v, _)) = defaults.get("default_transaction_isolation") {
            self.default_isolation = isolation_of(v);
            self.settings.insert("transaction_isolation".into(), v.clone());
        }
        if let Some((v, _)) = defaults.get("default_transaction_read_only") {
            self.settings.insert("transaction_read_only".into(), v.clone());
        }
        self.session_defaults = Arc::new(defaults);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_split_and_parse() {
        assert_eq!(split_options("  -c a=b\\ c   --x-y=1 "), ["-c", "a=b c", "--x-y=1"]);
        assert_eq!(split_options("a\\\\b"), ["a\\b"]);
        let parsed = parse_options("-c search_path=a,b --work-mem=2MB -cfoo.bar=x=y").expect("parse");
        assert_eq!(
            parsed,
            [
                ("search_path".to_owned(), "a,b".to_owned()),
                ("work_mem".to_owned(), "2MB".to_owned()),
                ("foo.bar".to_owned(), "x=y".to_owned())
            ]
        );
        let e = parse_options("-c search_path").expect_err("no value");
        assert_eq!((e.code, e.message.as_str()), ("42601", "-c search_path requires a value"));
        let e = parse_options("--work_mem").expect_err("no value");
        assert_eq!(e.message, "--work_mem requires a value");
        assert!(is_identifier_list("") && is_identifier_list(" a , \"b c\"\"d\",e") && is_identifier_list("\"$user\", public"));
        assert!(!is_identifier_list("a b,c") && !is_identifier_list("a,,b") && !is_identifier_list("a,") && !is_identifier_list("\"a"));
        let e = parse_options("search_path=x").expect_err("not a switch");
        assert_eq!(e.message, "invalid command-line argument for server process: search_path=x");
    }
}
