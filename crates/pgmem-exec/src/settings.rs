//! Configuration parameters (GUCs): their definitions as recorded from PostgreSQL 18's
//! `pg_settings`, value validation and normalization (`parse_and_validate_value`), the text
//! `SHOW` / `current_setting` print (`_ShowOption`, with units), and the context rules that
//! keep server-level parameters from changing.

use pgmem_types::{SqlError, SqlResult, code};
use std::sync::LazyLock;

/// One `pg_settings` row's metadata.
#[derive(Debug, Clone)]
pub struct Def {
    pub name: &'static str,
    pub unit: Option<&'static str>,
    pub category: &'static str,
    pub short_desc: Option<&'static str>,
    pub extra_desc: Option<&'static str>,
    pub context: &'static str,
    pub vartype: &'static str,
    pub min: Option<&'static str>,
    pub max: Option<&'static str>,
    pub enumvals: Vec<&'static str>,
    pub boot: Option<&'static str>,
}

const DATA: &str = include_str!("catalog_views/pg_settings.tsv");

static DEFS: LazyLock<Vec<Def>> = LazyLock::new(|| {
    DATA.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&'static str> = l.split('\t').collect();
            let opt = |i: usize| f.get(i).copied().filter(|v| *v != "\\N");
            let enumvals = opt(9)
                .map(|v| v.trim_start_matches('{').trim_end_matches('}').split(',').map(|s| s.trim_matches('"')).collect())
                .unwrap_or_default();
            Def {
                name: f[0],
                unit: opt(1),
                category: opt(2).unwrap_or(""),
                short_desc: opt(3),
                extra_desc: opt(4),
                context: opt(5).unwrap_or("user"),
                vartype: opt(6).unwrap_or("string"),
                min: opt(7),
                max: opt(8),
                enumvals,
                boot: opt(10),
            }
        })
        .collect()
});

/// Every parameter definition, by name.
pub fn defs() -> &'static [Def] {
    &DEFS
}

/// A parameter's definition (names are case-insensitive).
pub fn lookup(name: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.name.eq_ignore_ascii_case(name))
}

/// A placeholder (custom) parameter: `prefix.name`, any value, created on first SET.
pub fn is_custom(name: &str) -> bool {
    name.split_once('.').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
}

pub fn unrecognized(name: &str) -> SqlError {
    SqlError::new(code::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\""))
}

fn invalid_value(def: &Def, value: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"{}\": \"{value}\"", def.name))
}

/// Whether a session may change the parameter now (`set_config_option`'s context checks).
pub fn check_settable(def: &Def) -> SqlResult<()> {
    let msg = match def.context {
        "internal" => format!("parameter \"{}\" cannot be changed", def.name),
        "postmaster" => format!("parameter \"{}\" cannot be changed without restarting the server", def.name),
        "sighup" => format!("parameter \"{}\" cannot be changed now", def.name),
        "backend" | "superuser-backend" => format!("parameter \"{}\" cannot be set after connection start", def.name),
        _ => return Ok(()),
    };
    Err(SqlError::new(code::CANT_CHANGE_RUNTIME_PARAM, msg))
}

/// Size of one unit in a common base (bytes or microseconds), for parsing and printing.
fn unit_size(unit: &str) -> Option<(bool, f64)> {
    Some(match unit {
        "B" => (true, 1.0),
        "kB" => (true, 1024.0),
        "8kB" => (true, 8192.0),
        "16MB" => (true, 16.0 * 1_048_576.0),
        "MB" => (true, 1_048_576.0),
        "GB" => (true, 1_073_741_824.0),
        "TB" => (true, 1_099_511_627_776.0),
        "us" => (false, 1.0),
        "ms" => (false, 1000.0),
        "s" => (false, 1_000_000.0),
        "min" => (false, 60_000_000.0),
        "h" => (false, 3_600_000_000.0),
        "d" => (false, 86_400_000_000.0),
        _ => return None,
    })
}

const MEMORY_UNITS: &[&str] = &["TB", "GB", "MB", "kB", "B"];
const TIME_UNITS: &[&str] = &["d", "h", "min", "s", "ms", "us"];

/// `parse_int` / `parse_real` with an optional unit: the value in the parameter's base unit.
fn parse_number(def: &Def, raw: &str) -> SqlResult<f64> {
    let s = raw.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))).unwrap_or(s.len());
    let (num, unit) = (s[..split].trim(), s[split..].trim());
    // Hex / octal integer forms (0x10) are accepted by Postgres too.
    let mut value: f64 = if let Some(hex) = num.strip_prefix("0x").or_else(|| num.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).map_err(|_| invalid_value(def, raw))? as f64
    } else {
        num.parse().map_err(|_| invalid_value(def, raw))?
    };
    if !unit.is_empty() {
        let base = def.unit.and_then(unit_size).ok_or_else(|| invalid_value(def, raw))?;
        let given = unit_size(unit).filter(|g| g.0 == base.0).ok_or_else(|| {
            let valid =
                if base.0 { "\"B\", \"kB\", \"MB\", \"GB\", and \"TB\"" } else { "\"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\"" };
            invalid_value(def, raw).with_hint(format!("Valid units for this parameter are {valid}."))
        })?;
        value = value * given.1 / base.1;
    }
    Ok(value)
}

/// Postgres' range-error message: `-5 ms is outside the valid range for parameter
/// "statement_timeout" (0 ms .. 2147483647 ms)`.
fn out_of_range(def: &Def, value: &str) -> SqlError {
    let u = def.unit.map(|u| format!(" {u}")).unwrap_or_default();
    SqlError::new(
        code::INVALID_PARAMETER_VALUE,
        format!(
            "{value}{u} is outside the valid range for parameter \"{}\" ({}{u} .. {}{u})",
            def.name,
            def.min.unwrap_or(""),
            def.max.unwrap_or("")
        ),
    )
}

/// `parse_bool`: true/false/yes/no/on/off/1/0 and unambiguous prefixes.
pub fn parse_bool(s: &str) -> Option<bool> {
    let v = s.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix_of = |w: &str| w.starts_with(v.as_str());
    match v.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        "on" => Some(true),
        "of" | "off" => Some(false),
        _ if prefix_of("true") || prefix_of("yes") => Some(true),
        _ if prefix_of("false") || prefix_of("no") => Some(false),
        _ => None,
    }
}

/// Validates a value and returns the form it is stored in: integers in the base unit,
/// booleans as on/off, enum members in their canonical spelling, DateStyle normalized.
pub fn normalize(def: &Def, raw: &str, current: Option<&str>) -> SqlResult<String> {
    match def.vartype {
        "bool" => parse_bool(raw)
            .map(|b| if b { "on" } else { "off" }.to_owned())
            .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("parameter \"{}\" requires a Boolean value", def.name))),
        "integer" => {
            let v = parse_number(def, raw)?.round();
            let (min, max): (f64, f64) =
                (def.min.and_then(|m| m.parse().ok()).unwrap_or(f64::MIN), def.max.and_then(|m| m.parse().ok()).unwrap_or(f64::MAX));
            #[allow(clippy::cast_possible_truncation)]
            let shown = (v as i64).to_string();
            if v < min || v > max {
                return Err(out_of_range(def, &shown));
            }
            Ok(shown)
        }
        "real" => {
            let v = parse_number(def, raw)?;
            let (min, max): (f64, f64) =
                (def.min.and_then(|m| m.parse().ok()).unwrap_or(f64::MIN), def.max.and_then(|m| m.parse().ok()).unwrap_or(f64::MAX));
            if v < min || v > max {
                return Err(out_of_range(def, &pgmem_types::text::format_float8(v)));
            }
            Ok(pgmem_types::text::format_float8(v))
        }
        "enum" => {
            let v = raw.trim();
            def.enumvals
                .iter()
                .find(|e| e.eq_ignore_ascii_case(v))
                .map(|e| (*e).to_owned())
                .ok_or_else(|| invalid_value(def, raw).with_hint(format!("Available values: {}.", def.enumvals.join(", "))))
        }
        _ if def.name == "DateStyle" => datestyle(def, raw, current),
        _ => Ok(raw.to_owned()),
    }
}

/// `check_datestyle`: `ISO|SQL|Postgres|German` and `YMD|DMY|MDY` (with the European/US
/// aliases), each part optional; German alone implies DMY.
fn datestyle(def: &Def, raw: &str, current: Option<&str>) -> SqlResult<String> {
    let (mut style, mut order) = {
        let cur = current.unwrap_or("ISO, MDY");
        let mut parts = cur.split(',').map(str::trim);
        (parts.next().unwrap_or("ISO").to_owned(), parts.next().unwrap_or("MDY").to_owned())
    };
    let (mut have_style, mut have_order) = (false, false);
    let conflict = || invalid_value(def, raw).with_detail("Conflicting \"datestyle\" specifications.");
    for tok in raw.split([',', ' ']).map(str::trim).filter(|t| !t.is_empty()) {
        let up = tok.to_ascii_uppercase();
        let (is_style, val) = match up.as_str() {
            "ISO" => (true, "ISO"),
            "SQL" => (true, "SQL"),
            "POSTGRES" => (true, "Postgres"),
            "GERMAN" => (true, "German"),
            "YMD" => (false, "YMD"),
            "DMY" | "EURO" | "EUROPEAN" => (false, "DMY"),
            "MDY" | "US" | "NONEURO" | "NONEUROPEAN" => (false, "MDY"),
            "DEFAULT" => {
                style = "ISO".into();
                order = "MDY".into();
                continue;
            }
            _ => return Err(invalid_value(def, raw).with_detail(format!("Unrecognized key word: \"{tok}\"."))),
        };
        if is_style {
            if have_style && style != val {
                return Err(conflict());
            }
            have_style = true;
            style = val.to_owned();
        } else {
            if have_order && order != val {
                return Err(conflict());
            }
            have_order = true;
            order = val.to_owned();
        }
    }
    if have_style && !have_order && style == "German" {
        order = "DMY".into();
    }
    Ok(format!("{style}, {order}"))
}

/// `_ShowOption`: integers with a unit print in the largest unit that divides them evenly
/// (`4096` kB is `4MB`, `1500` ms stays `1500ms`).
pub fn show(def: Option<&Def>, stored: &str) -> String {
    let Some(def) = def else { return stored.to_owned() };
    if def.vartype != "integer" {
        return stored.to_owned();
    }
    let (Some(unit), Ok(v)) = (def.unit, stored.parse::<i64>()) else { return stored.to_owned() };
    if v <= 0 {
        return v.to_string();
    }
    let Some((memory, base)) = unit_size(unit) else { return stored.to_owned() };
    let total = v as f64 * base;
    let units = if memory { MEMORY_UNITS } else { TIME_UNITS };
    for u in units {
        let Some((_, size)) = unit_size(u) else { continue };
        if size < base {
            break;
        }
        if (total / size).fract() == 0.0 {
            #[allow(clippy::cast_possible_truncation)]
            return format!("{}{u}", (total / size) as i64);
        }
    }
    format!("{v}{unit}")
}
