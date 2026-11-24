//! Time zones: the session `TimeZone`, `AT TIME ZONE` targets and zone names in timestamp input,
//! resolved the way Postgres does (abbreviations from its `Default` set, IANA names from the
//! bundled tz database, POSIX-style offsets such as `UTC+5`), with Postgres' rules for local
//! times that fall in a DST gap or fold.

use crate::datetime::{PG_EPOCH_UNIX_SECS, USECS_PER_SEC};
use jiff::tz::{AmbiguousOffset, TimeZone};

/// Postgres' `Default` timezone abbreviation set: (abbreviation, seconds east, is_dst).
const ABBREVS: &str = include_str!("tz_abbrevs.tsv");

#[derive(Clone, Debug)]
enum Kind {
    /// A fixed offset in seconds east of UTC.
    Fixed(i32),
    Tz(TimeZone),
}

/// A resolved time zone and the name it reports (`SHOW TimeZone`).
#[derive(Clone, Debug)]
pub struct Zone {
    pub name: String,
    kind: Kind,
}

impl PartialEq for Zone {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

/// Postgres epoch microseconds → a jiff timestamp (clamped to jiff's range).
fn to_jiff(utc_micros: i64) -> jiff::Timestamp {
    let secs = utc_micros.div_euclid(USECS_PER_SEC).saturating_add(PG_EPOCH_UNIX_SECS);
    jiff::Timestamp::from_second(secs.clamp(jiff::Timestamp::MIN.as_second(), jiff::Timestamp::MAX.as_second()))
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// Offset text in Postgres' style: `+05`, `-04:30`, `-04:56:02`.
pub fn format_offset(east_secs: i32) -> String {
    let sign = if east_secs < 0 { '-' } else { '+' };
    let abs = east_secs.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs % 3600) / 60, abs % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

/// Parses `[+-]H[H][:MM[:SS]]` or `[+-]HHMM` into seconds (the sign as written).
fn parse_hms(s: &str) -> Option<i32> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        b'0'..=b'9' => (1, s),
        _ => return None,
    };
    let (h, m, sec) = if let Some((h, rest)) = digits.split_once(':') {
        let (m, sec) = rest.split_once(':').unwrap_or((rest, "0"));
        (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?, sec.parse::<i32>().ok()?)
    } else {
        match digits.len() {
            1 | 2 => (digits.parse().ok()?, 0, 0),
            4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?, 0),
            6 => (digits[..2].parse().ok()?, digits[2..4].parse().ok()?, digits[4..].parse().ok()?),
            _ => return None,
        }
    };
    if !digits.bytes().all(|b| b.is_ascii_digit() || b == b':') || h > 167 || m > 59 || sec > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

impl Zone {
    pub fn utc() -> Zone {
        Zone { name: "UTC".into(), kind: Kind::Fixed(0) }
    }

    /// A fixed offset, named the way `SET TIME ZONE 5` reports it (`<+05>-05`).
    pub fn fixed(east_secs: i32) -> Zone {
        let off = format_offset(east_secs);
        let posix = format_offset(-east_secs);
        Zone { name: format!("<{off}>{posix}"), kind: Kind::Fixed(east_secs) }
    }

    /// An IANA zone by name, case-insensitively, reporting its canonical spelling.
    fn named(name: &str) -> Option<Zone> {
        let tz = TimeZone::get(name).ok()?;
        let canonical = tz.iana_name().map_or_else(|| name.to_owned(), str::to_owned);
        Some(Zone { name: canonical, kind: Kind::Tz(tz) })
    }

    /// A POSIX-style zone without DST rules: `UTC+5`, `<+05>-05`, `+03:00` (a positive offset
    /// is *west* of Greenwich, as in POSIX).
    fn posix(spec: &str) -> Option<Zone> {
        let s = spec.trim();
        let rest = if let Some(inner) = s.strip_prefix('<') {
            &inner[inner.find('>')? + 1..]
        } else {
            let name_len = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
            if name_len != 0 && name_len < 3 {
                return None;
            }
            &s[name_len..]
        };
        if rest.is_empty() {
            return None;
        }
        let west = parse_hms(rest)?;
        Some(Zone { name: s.to_owned(), kind: Kind::Fixed(-west) })
    }

    /// A `TimeZone` setting value (`SET TIME ZONE ...`): an IANA name or a POSIX-style spec.
    pub fn from_setting(value: &str) -> Option<Zone> {
        let v = value.trim();
        if v.is_empty() {
            return None;
        }
        if v.eq_ignore_ascii_case("utc") {
            return Some(Zone::utc());
        }
        Zone::named(v).or_else(|| Zone::posix(v))
    }

    /// A zone named in `AT TIME ZONE` or `timezone()`: an abbreviation first, then a setting value.
    pub fn from_spec(spec: &str) -> Option<Zone> {
        abbreviation(spec).map(|(secs, _)| Zone { name: spec.to_owned(), kind: Kind::Fixed(secs) }).or_else(|| Zone::from_setting(spec))
    }

    /// The offset (seconds east of UTC) in effect at a UTC instant.
    pub fn offset_at(&self, utc_micros: i64) -> i32 {
        match &self.kind {
            Kind::Fixed(s) => *s,
            Kind::Tz(tz) => tz.to_offset(to_jiff(utc_micros)).seconds(),
        }
    }

    /// The zone abbreviation in effect at a UTC instant (`EST`, `CEST`, or `+0530`-style).
    pub fn abbrev_at(&self, utc_micros: i64) -> String {
        match &self.kind {
            // A POSIX-style zone's abbreviation is its standard-time name (`UTC` of `UTC+5`,
            // `+05` of `<+05>-05`); an abbreviation zone reports itself.
            Kind::Fixed(s) => {
                if let Some(inner) = self.name.strip_prefix('<') {
                    inner.split('>').next().unwrap_or_default().to_owned()
                } else {
                    let letters: String = self.name.chars().take_while(char::is_ascii_alphabetic).collect();
                    if letters.is_empty() { format_offset(*s) } else { letters.to_ascii_uppercase() }
                }
            }
            Kind::Tz(tz) => tz.to_offset_info(to_jiff(utc_micros)).abbreviation().to_owned(),
        }
    }

    /// The UTC instant of a local wall-clock time in this zone. A time skipped by a DST
    /// change takes the offset in effect before it; an ambiguous one the offset after it
    /// (standard time), as Postgres' `DetermineTimeZoneOffset` does.
    pub fn utc_of_local(&self, local_micros: i64) -> i64 {
        let off = match &self.kind {
            Kind::Fixed(s) => *s,
            Kind::Tz(tz) => {
                // Treat the local time as if it were UTC to get its civil fields.
                let civil = to_jiff(local_micros).to_zoned(TimeZone::UTC).datetime();
                match tz.to_ambiguous_timestamp(civil).offset() {
                    AmbiguousOffset::Unambiguous { offset } => offset.seconds(),
                    AmbiguousOffset::Gap { before, .. } => before.seconds(),
                    AmbiguousOffset::Fold { after, .. } => after.seconds(),
                }
            }
        };
        local_micros - i64::from(off) * USECS_PER_SEC
    }

    /// Local wall-clock microseconds of a UTC instant.
    pub fn local_of_utc(&self, utc_micros: i64) -> i64 {
        utc_micros + i64::from(self.offset_at(utc_micros)) * USECS_PER_SEC
    }

    pub fn is_utc(&self) -> bool {
        matches!(self.kind, Kind::Fixed(0))
    }
}

fn infinite(t: i64) -> bool {
    t == i64::MAX || t == i64::MIN
}

/// The session zone's local wall-clock time of a timestamptz (infinities pass through).
pub fn to_local(utc_micros: i64) -> i64 {
    if infinite(utc_micros) { utc_micros } else { crate::context::with_zone(|z| z.local_of_utc(utc_micros)) }
}

/// The timestamptz of a local wall-clock time in the session zone.
pub fn from_local(local_micros: i64) -> i64 {
    if infinite(local_micros) { local_micros } else { crate::context::with_zone(|z| z.utc_of_local(local_micros)) }
}

/// The session zone's offset (seconds east) at a UTC instant.
pub fn session_offset(utc_micros: i64) -> i32 {
    crate::context::with_zone(|z| z.offset_at(utc_micros))
}

/// A timezone abbreviation from Postgres' `Default` set: (seconds east, is_dst).
pub fn abbreviation(name: &str) -> Option<(i32, bool)> {
    ABBREVS.lines().find_map(|l| {
        let mut f = l.split('\t');
        let abbrev = f.next()?;
        if !abbrev.eq_ignore_ascii_case(name) {
            return None;
        }
        Some((f.next()?.parse().ok()?, f.next()? == "t"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::{USECS_PER_HOUR, parse_timestamptz};

    fn ts(s: &str) -> i64 {
        parse_timestamptz(s).expect("timestamp")
    }

    #[test]
    fn settings_and_names() {
        assert_eq!(Zone::from_setting("america/new_york").map(|z| z.name).as_deref(), Some("America/New_York"));
        assert_eq!(Zone::fixed(5 * 3600).name, "<+05>-05");
        assert_eq!(Zone::fixed(5 * 3600 + 1800).name, "<+05:30>-05:30");
        assert_eq!(Zone::from_setting("UTC+5").map(|z| z.offset_at(0)), Some(-5 * 3600));
        assert!(Zone::from_setting("Nope/Zone").is_none());
        assert_eq!(Zone::from_spec("EST").map(|z| z.offset_at(0)), Some(-5 * 3600));
        assert_eq!(Zone::from_spec("+03:00").map(|z| z.offset_at(0)), Some(-3 * 3600));
    }

    #[test]
    fn dst_gap_and_fold_follow_postgres() {
        let ny = Zone::from_setting("America/New_York").expect("zone");
        // 02:30 on 2024-03-10 doesn't exist: it is read with the pre-transition offset (-05).
        let local = ts("2024-03-10 02:30:00+00");
        assert_eq!(ny.utc_of_local(local), local + 5 * USECS_PER_HOUR);
        // 01:30 on 2024-11-03 happens twice: Postgres picks standard time (-05).
        let local = ts("2024-11-03 01:30:00+00");
        assert_eq!(ny.utc_of_local(local), local + 5 * USECS_PER_HOUR);
        assert_eq!(ny.offset_at(ts("2024-07-01 12:00:00+00")), -4 * 3600);
        assert_eq!(format_offset(ny.offset_at(ts("1883-01-01 00:00:00+00"))), "-04:56:02");
    }
}
