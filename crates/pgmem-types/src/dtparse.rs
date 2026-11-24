//! Date/time input: a port of Postgres' `ParseDateTime` (splitting the string into typed
//! fields) and `DecodeDateTime` / `DecodeTimeOnly` (assigning them to date, time and zone
//! parts). Field order for ambiguous numeric dates follows `DateStyle` (`1/2/2024` is January
//! 2nd under MDY), month and weekday names, `AM`/`PM`, `BC`, zone abbreviations, IANA zone
//! names, ISO-8601 `T`, run-together `20240315` / `101500`, Julian `J2451545` and the special
//! words (`now`, `today`, `epoch`, `infinity`, `allballs`) are all accepted.

use crate::calendar::{self, Tm};
use crate::context::DateOrder;
use crate::datetime::{USECS_PER_DAY, USECS_PER_SEC};
use crate::zone::Zone;
use crate::{SqlError, SqlResult, code};

/// What a string decoded to.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// Broken-down fields; `zone` is the explicit offset (seconds east of UTC), if any.
    Fields {
        tm: Tm,
        has_date: bool,
        has_time: bool,
        zone: Option<ZoneSpec>,
    },
    Epoch,
    Infinity,
    NegInfinity,
}

/// An explicit zone: a fixed offset, or a named zone whose offset depends on the date.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneSpec {
    Offset(i32),
    Named(Zone),
}

impl ZoneSpec {
    /// The offset (seconds east) for a local time in this zone.
    pub fn offset_for_local(&self, local: i64) -> i32 {
        match self {
            ZoneSpec::Offset(s) => *s,
            ZoneSpec::Named(z) => i32::try_from((local - z.utc_of_local(local)) / USECS_PER_SEC).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ftype {
    Number,
    Str,
    Date,
    Time,
    Tz,
    Special,
}

/// Errors as Postgres' `DateTimeParseError` reports them.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DtErr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    TzDispOverflow,
    BadTimezone,
}

fn report(e: DtErr, input: &str, type_name: &str, bad_zone: &str) -> SqlError {
    match e {
        DtErr::BadFormat => SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type {type_name}: \"{input}\"")),
        DtErr::FieldOverflow => SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("date/time field value out of range: \"{input}\"")),
        DtErr::MdFieldOverflow => SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("date/time field value out of range: \"{input}\""))
            .with_hint("Perhaps you need a different \"DateStyle\" setting."),
        DtErr::TzDispOverflow => {
            SqlError::new(code::INVALID_TIME_ZONE_DISPLACEMENT_VALUE, format!("time zone displacement out of range: \"{input}\""))
        }
        DtErr::BadTimezone => SqlError::new(code::INVALID_PARAMETER_VALUE, format!("time zone \"{bad_zone}\" not recognized")),
    }
}

/// `ParseDateTime`: the string's fields, lower-cased, with their types.
fn split_fields(s: &str) -> Result<Vec<(String, Ftype)>, DtErr> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let take = |i: &mut usize, buf: &mut String, f: &dyn Fn(u8) -> bool| {
        while *i < b.len() && f(b[*i]) {
            buf.push(char::from(b[*i].to_ascii_lowercase()));
            *i += 1;
        }
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let mut buf = String::new();
        let ftype;
        if c.is_ascii_digit() {
            take(&mut i, &mut buf, &|c| c.is_ascii_digit());
            match b.get(i) {
                Some(b':') => {
                    ftype = Ftype::Time;
                    take(&mut i, &mut buf, &|c| c.is_ascii_digit() || c == b':' || c == b'.');
                }
                Some(&d) if d == b'-' || d == b'/' || d == b'.' => {
                    buf.push(char::from(d));
                    i += 1;
                    if b.get(i).is_some_and(u8::is_ascii_digit) {
                        let mut t = if d == b'.' { Ftype::Number } else { Ftype::Date };
                        take(&mut i, &mut buf, &|c| c.is_ascii_digit());
                        if b.get(i) == Some(&d) {
                            t = Ftype::Date;
                            take(&mut i, &mut buf, &|c| c.is_ascii_digit() || c == d);
                        }
                        ftype = t;
                    } else {
                        ftype = Ftype::Date;
                        take(&mut i, &mut buf, &|c| c.is_ascii_alphanumeric() || c == d);
                    }
                }
                _ => ftype = Ftype::Number,
            }
        } else if c == b'.' {
            buf.push('.');
            i += 1;
            take(&mut i, &mut buf, &|c| c.is_ascii_digit());
            ftype = Ftype::Number;
        } else if c.is_ascii_alphabetic() {
            take(&mut i, &mut buf, &|c| c.is_ascii_alphabetic());
            let next = b.get(i).copied();
            let is_date = match next {
                Some(b'-' | b'/' | b'.') => true,
                Some(n) if n == b'+' || n.is_ascii_digit() => lookup(&buf).is_none(),
                _ => false,
            };
            if is_date {
                ftype = Ftype::Date;
                take(&mut i, &mut buf, &|c| matches!(c, b'+' | b'-' | b'/' | b'_' | b'.' | b':') || c.is_ascii_alphanumeric());
            } else {
                ftype = Ftype::Str;
            }
        } else if c == b'+' || c == b'-' {
            buf.push(char::from(c));
            i += 1;
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            match b.get(i) {
                Some(d) if d.is_ascii_digit() => {
                    ftype = Ftype::Tz;
                    take(&mut i, &mut buf, &|c| c.is_ascii_digit() || matches!(c, b':' | b'.' | b'-'));
                }
                Some(a) if a.is_ascii_alphabetic() => {
                    ftype = Ftype::Special;
                    take(&mut i, &mut buf, &|c| c.is_ascii_alphabetic());
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else if c.is_ascii_punctuation() {
            i += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        out.push((buf, ftype));
    }
    Ok(out)
}

/// Tokens of Postgres' `datetktbl`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Month(u32),
    Dow,
    Am,
    Pm,
    Ad,
    Bc,
    Now,
    Today,
    Tomorrow,
    Yesterday,
    Epoch,
    Infinity,
    NegInfinity,
    Allballs,
    Ignore,
    IsoTime,
    Julian,
    Utc,
}

fn lookup(word: &str) -> Option<Tok> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    const FULL: [&str; 12] =
        ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    if let Some(m) = MONTHS.iter().position(|m| *m == word).or_else(|| FULL.iter().position(|m| *m == word)) {
        return Some(Tok::Month(u32::try_from(m).unwrap_or(0) + 1));
    }
    Some(match word {
        "sept" => Tok::Month(9),
        "sun" | "sunday" | "mon" | "monday" | "tue" | "tues" | "tuesday" | "wed" | "wednesday" | "thu" | "thur" | "thurs" | "thursday"
        | "fri" | "friday" | "sat" | "saturday" => Tok::Dow,
        "am" | "a.m." => Tok::Am,
        "pm" | "p.m." => Tok::Pm,
        "ad" => Tok::Ad,
        "bc" => Tok::Bc,
        "now" => Tok::Now,
        "today" => Tok::Today,
        "tomorrow" => Tok::Tomorrow,
        "yesterday" => Tok::Yesterday,
        "epoch" => Tok::Epoch,
        "infinity" | "+infinity" => Tok::Infinity,
        "-infinity" => Tok::NegInfinity,
        "allballs" => Tok::Allballs,
        "at" | "on" => Tok::Ignore,
        "t" => Tok::IsoTime,
        "j" | "jd" | "julian" => Tok::Julian,
        "z" | "zulu" => Tok::Utc,
        _ => return None,
    })
}

const YEAR: u32 = 1;
const MONTH: u32 = 2;
const DAY: u32 = 4;
const DATE_M: u32 = YEAR | MONTH | DAY;
const HOUR: u32 = 8;
const MINUTE: u32 = 16;
const SECOND: u32 = 32;
const TIME_M: u32 = HOUR | MINUTE | SECOND;
const TZ: u32 = 64;
const DOY: u32 = 128;
const AMPM: u32 = 256;
const DOW: u32 = 512;
const ADBC: u32 = 1024;

/// The fields being assembled (`struct pg_tm` plus `fsec`).
#[derive(Default)]
struct State {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
    doy: i64,
    fmask: u32,
    is2digits: bool,
    bc: bool,
    pm: Option<bool>,
    zone: Option<ZoneSpec>,
    special: Option<Decoded>,
    order: DateOrder,
}

fn atoi(s: &str) -> Result<i64, DtErr> {
    if s.is_empty() {
        return Ok(0);
    }
    s.parse().map_err(|_| DtErr::BadFormat)
}

/// `ParseFractionalSecond`: `.123456789` to rounded microseconds.
fn fraction(s: &str) -> Result<i64, DtErr> {
    let f: f64 = format!("0{s}").parse().map_err(|_| DtErr::BadFormat)?;
    #[allow(clippy::cast_possible_truncation)]
    Ok((f * 1_000_000.0).round() as i64)
}

impl State {
    /// `DecodeNumberField`: run-together `YYYYMMDD`, `HHMMSS`, `HHMM`, with an optional fraction.
    fn number_field(&mut self, s: &str) -> Result<u32, DtErr> {
        let (digits, frac) = match s.find('.') {
            Some(p) => (&s[..p], Some(&s[p..])),
            None => (s, None),
        };
        let len = digits.len();
        if let Some(f) = frac {
            self.fsec = fraction(f)?;
        } else if self.fmask & DATE_M != DATE_M && len >= 6 {
            self.mday = atoi(&digits[len - 2..])?;
            self.mon = atoi(&digits[len - 4..len - 2])?;
            self.year = atoi(&digits[..len - 4])?;
            if len - 4 == 2 {
                self.is2digits = true;
            }
            return Ok(DATE_M);
        }
        if self.fmask & TIME_M != TIME_M {
            if len == 6 {
                self.sec = atoi(&digits[4..])?;
                self.min = atoi(&digits[2..4])?;
                self.hour = atoi(&digits[..2])?;
                return Ok(TIME_M);
            } else if len == 4 {
                self.sec = 0;
                self.min = atoi(&digits[2..])?;
                self.hour = atoi(&digits[..2])?;
                return Ok(TIME_M);
            }
        }
        Err(DtErr::BadFormat)
    }

    /// `DecodeNumber`: one numeric field placed by what is already known and `DateStyle`.
    fn number(&mut self, s: &str, text_month: bool, fmask: u32) -> Result<u32, DtErr> {
        let flen = s.len();
        let (int, rest) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(flen));
        let val = atoi(int)?;
        if let Some(r) = rest.strip_prefix('.') {
            if int.len() > 2 {
                let saved = self.fmask;
                self.fmask = fmask | DATE_M;
                let t = self.number_field(s);
                self.fmask = saved;
                return t;
            }
            self.fsec = fraction(&format!(".{r}"))?;
        } else if !rest.is_empty() {
            return Err(DtErr::BadFormat);
        }
        if flen == 3 && fmask & DATE_M == YEAR && (1..=366).contains(&val) {
            self.doy = val;
            return Ok(DOY | MONTH | DAY);
        }
        let ymd = self.order == DateOrder::Ymd;
        let tmask = match fmask & DATE_M {
            0 if flen >= 3 || ymd => YEAR,
            0 if self.order == DateOrder::Dmy => DAY,
            0 => MONTH,
            YEAR => MONTH,
            MONTH if text_month && (flen >= 3 || ymd) => YEAR,
            MONTH => DAY,
            m if m == YEAR | MONTH => {
                if text_month && flen >= 3 && self.is2digits {
                    self.mday = self.year;
                    self.year = val;
                    self.is2digits = false;
                    return Ok(DAY);
                }
                DAY
            }
            DAY => MONTH,
            m if m == MONTH | DAY => YEAR,
            DATE_M => return self.number_field(s),
            _ => return Err(DtErr::BadFormat),
        };
        match tmask {
            YEAR => {
                self.year = val;
                self.is2digits = flen <= 2;
            }
            MONTH => self.mon = val,
            _ => self.mday = val,
        }
        Ok(tmask)
    }

    /// `DecodeDate`: a delimited date (`2024-03-15`, `15/03/2024`, `15-mar-2024`).
    fn date(&mut self, s: &str) -> Result<u32, DtErr> {
        let mut fields = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_alphanumeric() {
                i += 1;
                continue;
            }
            let start = i;
            let digit = b[i].is_ascii_digit();
            while i < b.len() && (if digit { b[i].is_ascii_digit() } else { b[i].is_ascii_alphabetic() }) {
                i += 1;
            }
            fields.push(&s[start..i]);
        }
        let mut fmask = self.fmask;
        let mut tmask = 0;
        let mut text_month = false;
        let mut numeric = Vec::new();
        for f in fields {
            if f.as_bytes()[0].is_ascii_alphabetic() {
                match lookup(f) {
                    Some(Tok::Ignore) => continue,
                    Some(Tok::Month(m)) => {
                        if fmask & MONTH != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        self.mon = i64::from(m);
                        text_month = true;
                        fmask |= MONTH;
                        tmask |= MONTH;
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            } else {
                numeric.push(f);
            }
        }
        for f in numeric {
            let dmask = self.number(f, text_month, fmask)?;
            if fmask & dmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= dmask;
            tmask |= dmask;
        }
        if fmask & DATE_M != DATE_M {
            return Err(DtErr::BadFormat);
        }
        Ok(tmask)
    }

    /// `DecodeTime`: `hh:mm[:ss[.fff]]`.
    fn time(&mut self, s: &str) -> Result<u32, DtErr> {
        let parts: Vec<&str> = s.split(':').collect();
        let num = |p: &str| -> Result<i64, DtErr> {
            if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
                return Err(DtErr::BadFormat);
            }
            atoi(p)
        };
        match parts.as_slice() {
            [h, m] => {
                self.hour = num(h)?;
                match m.split_once('.') {
                    // `mm.fff` is minutes and seconds fraction only in interval input; here it
                    // is a bad format, like Postgres' timestamp path.
                    Some(_) => return Err(DtErr::BadFormat),
                    None => self.min = num(m)?,
                }
                self.sec = 0;
                self.fsec = 0;
            }
            [h, m, sec] => {
                self.hour = num(h)?;
                self.min = num(m)?;
                match sec.split_once('.') {
                    Some((w, f)) => {
                        self.sec = num(w)?;
                        self.fsec = if f.is_empty() { 0 } else { fraction(&format!(".{f}"))? };
                    }
                    None => {
                        self.sec = num(sec)?;
                        self.fsec = 0;
                    }
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if self.hour < 0 || !(0..60).contains(&self.min) || !(0..=60).contains(&self.sec) || !(0..=USECS_PER_SEC).contains(&self.fsec) {
            return Err(DtErr::FieldOverflow);
        }
        Ok(TIME_M)
    }

    fn time_overflows(&self) -> bool {
        let total = ((self.hour * 60 + self.min) * 60 + self.sec) * USECS_PER_SEC + self.fsec;
        self.hour > 24 || total > USECS_PER_DAY
    }

    /// Sets the date part from a day count (days since 2000-01-01).
    fn set_days(&mut self, days: i64) {
        let (y, m, d) = calendar::ymd_from_days(days);
        self.year = i64::from(y);
        self.mon = i64::from(m);
        self.mday = i64::from(d);
    }

    /// `ValidateDate`.
    fn validate_date(&mut self) -> Result<(), DtErr> {
        let fmask = self.fmask;
        if fmask & YEAR != 0 {
            if self.bc {
                if self.year <= 0 {
                    return Err(DtErr::FieldOverflow);
                }
                self.year = -(self.year - 1);
            } else if self.is2digits {
                if self.year < 0 {
                    return Err(DtErr::FieldOverflow);
                }
                if self.year < 70 {
                    self.year += 2000;
                } else if self.year < 100 {
                    self.year += 1900;
                }
            } else if self.year <= 0 {
                return Err(DtErr::FieldOverflow);
            }
        }
        if fmask & DOY != 0 {
            let y = i32::try_from(self.year).map_err(|_| DtErr::FieldOverflow)?;
            let jan1 = calendar::days_from_ymd(y, 1, 1);
            self.set_days(jan1 + self.doy - 1);
        }
        if fmask & MONTH != 0 && !(1..=12).contains(&self.mon) {
            return Err(DtErr::MdFieldOverflow);
        }
        if fmask & DAY != 0 && !(1..=31).contains(&self.mday) {
            return Err(DtErr::MdFieldOverflow);
        }
        if fmask & DATE_M == DATE_M {
            let (y, m, d) = (
                i32::try_from(self.year).map_err(|_| DtErr::FieldOverflow)?,
                u32::try_from(self.mon).map_err(|_| DtErr::FieldOverflow)?,
                u32::try_from(self.mday).map_err(|_| DtErr::FieldOverflow)?,
            );
            if !calendar::valid_date(y, m, d) {
                return Err(DtErr::FieldOverflow);
            }
        }
        Ok(())
    }
}

/// `DecodeTimezone`: `+hh`, `+hhmm`, `+hh:mm[:ss]` (seconds east).
fn decode_offset(s: &str) -> Result<i32, DtErr> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => return Err(DtErr::BadFormat),
    };
    let digits_end = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
    let mut hr = atoi(&body[..digits_end])?;
    let rest = &body[digits_end..];
    let (mut min, mut sec) = (0, 0);
    if let Some(r) = rest.strip_prefix(':') {
        let mut parts = r.split(':');
        let m = parts.next().unwrap_or("");
        if m.is_empty() || !m.bytes().all(|c| c.is_ascii_digit()) {
            return Err(DtErr::BadFormat);
        }
        min = atoi(m)?;
        if let Some(sp) = parts.next() {
            if sp.is_empty() || !sp.bytes().all(|c| c.is_ascii_digit()) {
                return Err(DtErr::BadFormat);
            }
            sec = atoi(sp)?;
        }
        if parts.next().is_some() {
            return Err(DtErr::BadFormat);
        }
    } else if rest.is_empty() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    } else if !rest.is_empty() {
        return Err(DtErr::BadFormat);
    }
    if !(0..=15).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DtErr::TzDispOverflow);
    }
    let tz = i32::try_from((hr * 60 + min) * 60 + sec).map_err(|_| DtErr::TzDispOverflow)?;
    Ok(if neg { -tz } else { tz })
}

/// A zone name or abbreviation (`est`, `utc`, `america/new_york`).
fn named_zone(name: &str) -> Option<ZoneSpec> {
    if let Some((secs, _)) = crate::zone::abbreviation(name) {
        return Some(ZoneSpec::Offset(secs));
    }
    Zone::named_ci(name).map(ZoneSpec::Named)
}

/// Decodes a date/time string. `time_only` is `DecodeTimeOnly` (the `time` types): the date
/// is optional and `now` / `today` keep only what applies.
fn decode(input: &str, time_only: bool) -> Result<State, (DtErr, String)> {
    let ctx = crate::context::current();
    let fields = split_fields(input).map_err(|e| (e, String::new()))?;
    let mut st = State { order: ctx.date_order, ..State::default() };
    let mut ptype: Option<Tok> = None;
    let bad = |e| (e, String::new());
    let mut text_month = false;
    for (i, (field, ftype)) in fields.iter().enumerate() {
        let tmask: u32;
        match ftype {
            Ftype::Date => {
                if ptype == Some(Tok::Julian) {
                    return Err(bad(DtErr::BadFormat));
                }
                if ptype.is_some() || st.fmask & (MONTH | DAY) == (MONTH | DAY) || (time_only && field.as_bytes()[0].is_ascii_alphabetic())
                {
                    if field.as_bytes()[0].is_ascii_digit() {
                        return Err(bad(DtErr::BadFormat));
                    }
                    st.zone = Some(named_zone(field).ok_or((DtErr::BadTimezone, field.clone()))?);
                    tmask = TZ;
                } else {
                    tmask = st.date(field).map_err(bad)?;
                    text_month |= field.bytes().any(|c| c.is_ascii_alphabetic());
                }
            }
            Ftype::Time => {
                if ptype.is_some() {
                    if ptype != Some(Tok::IsoTime) {
                        return Err(bad(DtErr::BadFormat));
                    }
                    ptype = None;
                }
                tmask = st.time(field).map_err(bad)?;
                if st.time_overflows() {
                    return Err(bad(DtErr::FieldOverflow));
                }
            }
            Ftype::Tz => {
                st.zone = Some(ZoneSpec::Offset(decode_offset(field).map_err(bad)?));
                tmask = TZ;
            }
            Ftype::Number => {
                if let Some(p) = ptype.take() {
                    match p {
                        Tok::Julian => {
                            let jd = atoi(field.split('.').next().unwrap_or("")).map_err(bad)?;
                            st.set_days(jd - 2_451_545);
                            tmask = DATE_M;
                        }
                        Tok::IsoTime => {
                            tmask = st.number_field(field).map_err(bad)?;
                            if tmask & TIME_M == 0 {
                                return Err(bad(DtErr::BadFormat));
                            }
                        }
                        _ => return Err(bad(DtErr::BadFormat)),
                    }
                } else {
                    let flen = field.len();
                    let dot = field.find('.');
                    tmask = if time_only && (dot.is_some() || flen > 4) {
                        // `DecodeTimeOnly`: run-together digits are a time.
                        let saved = st.fmask;
                        st.fmask |= DATE_M;
                        let t = st.number_field(field);
                        st.fmask = saved;
                        t.map_err(bad)?
                    } else if dot.is_some() && st.fmask & DATE_M == 0 && !time_only {
                        st.date(field).map_err(bad)?
                    } else if dot.is_some_and(|p| p > 2) || (flen >= 6 && (st.fmask & DATE_M == 0 || st.fmask & TIME_M == 0)) {
                        // A concatenated date or time (`20240315`, `101530`, `040506.789`).
                        st.number_field(field).map_err(bad)?
                    } else {
                        st.number(field, text_month, st.fmask).map_err(bad)?
                    };
                }
            }
            Ftype::Str | Ftype::Special => {
                if let Some((secs, _)) = crate::zone::abbreviation(field) {
                    st.zone = Some(ZoneSpec::Offset(secs));
                    tmask = TZ;
                } else {
                    match lookup(field) {
                        None => {
                            let z = Zone::named_ci(field).ok_or_else(|| bad(DtErr::BadFormat))?;
                            st.zone = Some(ZoneSpec::Named(z));
                            tmask = TZ;
                        }
                        Some(tok) => tmask = apply_token(&mut st, tok, &mut ptype, &mut text_month, &ctx, i, &fields).map_err(bad)?,
                    }
                }
            }
        }
        if st.fmask & tmask != 0 {
            return Err(bad(DtErr::BadFormat));
        }
        st.fmask |= tmask;
    }
    if ptype.is_some() {
        return Err(bad(DtErr::BadFormat));
    }
    if st.special.is_some() {
        return Ok(st);
    }
    st.validate_date().map_err(bad)?;
    if let Some(pm) = st.pm {
        if st.hour > 12 {
            return Err(bad(DtErr::FieldOverflow));
        }
        if !pm && st.hour == 12 {
            st.hour = 0;
        } else if pm && st.hour != 12 {
            st.hour += 12;
        }
    }
    Ok(st)
}

#[allow(clippy::too_many_arguments)]
fn apply_token(
    st: &mut State,
    tok: Tok,
    ptype: &mut Option<Tok>,
    text_month: &mut bool,
    ctx: &crate::context::Context,
    i: usize,
    fields: &[(String, Ftype)],
) -> Result<u32, DtErr> {
    let now_local = ctx.zone.local_of_utc(ctx.now);
    let today = now_local.div_euclid(USECS_PER_DAY);
    Ok(match tok {
        Tok::Month(m) => {
            // A numeric "month" already seen (`15 March`) was really the day.
            let tmask = if st.fmask & MONTH != 0 && !*text_month && st.fmask & DAY == 0 && (1..=31).contains(&st.mon) {
                st.mday = st.mon;
                DAY
            } else {
                MONTH
            };
            st.mon = i64::from(m);
            *text_month = true;
            tmask
        }
        Tok::Dow => DOW,
        Tok::Am | Tok::Pm => {
            st.pm = Some(tok == Tok::Pm);
            AMPM
        }
        Tok::Ad | Tok::Bc => {
            st.bc = tok == Tok::Bc;
            ADBC
        }
        Tok::Ignore => 0,
        Tok::Utc => {
            st.zone = Some(ZoneSpec::Offset(0));
            TZ
        }
        Tok::IsoTime | Tok::Julian => {
            let next_ok = fields.get(i + 1).is_some_and(|(_, t)| matches!(t, Ftype::Number | Ftype::Time | Ftype::Date));
            if tok == Tok::IsoTime && !next_ok {
                return Err(DtErr::BadFormat);
            }
            *ptype = Some(tok);
            0
        }
        Tok::Now => {
            let tm = Tm::from_micros(now_local);
            st.year = i64::from(tm.year);
            st.mon = i64::from(tm.month);
            st.mday = i64::from(tm.day);
            st.hour = i64::from(tm.hour);
            st.min = i64::from(tm.minute);
            st.sec = i64::from(tm.second);
            st.fsec = i64::from(tm.micros);
            st.zone = Some(ZoneSpec::Offset(ctx.zone.offset_at(ctx.now)));
            DATE_M | TIME_M | TZ
        }
        Tok::Today | Tok::Tomorrow | Tok::Yesterday => {
            let delta = match tok {
                Tok::Tomorrow => 1,
                Tok::Yesterday => -1,
                _ => 0,
            };
            st.set_days(today + delta);
            st.hour = 0;
            st.min = 0;
            st.sec = 0;
            st.fsec = 0;
            DATE_M
        }
        Tok::Allballs => {
            st.hour = 0;
            st.min = 0;
            st.sec = 0;
            st.fsec = 0;
            st.zone = Some(ZoneSpec::Offset(0));
            TIME_M | TZ
        }
        Tok::Epoch | Tok::Infinity | Tok::NegInfinity => {
            if fields.len() > 1 {
                return Err(DtErr::BadFormat);
            }
            st.special = Some(match tok {
                Tok::Epoch => Decoded::Epoch,
                Tok::Infinity => Decoded::Infinity,
                _ => Decoded::NegInfinity,
            });
            DATE_M | TIME_M
        }
    })
}

fn finish(st: State) -> Decoded {
    if let Some(s) = st.special {
        return s;
    }
    let tm = Tm {
        year: i32::try_from(st.year).unwrap_or(i32::MAX),
        month: u32::try_from(st.mon).unwrap_or(1).max(1),
        day: u32::try_from(st.mday).unwrap_or(1).max(1),
        hour: u32::try_from(st.hour).unwrap_or(0),
        minute: u32::try_from(st.min).unwrap_or(0),
        second: u32::try_from(st.sec).unwrap_or(0),
        micros: u32::try_from(st.fsec).unwrap_or(0),
    };
    Decoded::Fields { tm, has_date: st.fmask & DATE_M == DATE_M, has_time: st.fmask & TIME_M != 0, zone: st.zone }
}

/// `DecodeDateTime` for `date`, `timestamp` and `timestamptz` input: the full date is required.
pub fn decode_datetime(input: &str, type_name: &str) -> SqlResult<Decoded> {
    let st = decode(input, false).map_err(|(e, z)| report(e, input, type_name, &z))?;
    let d = finish(st);
    if let Decoded::Fields { has_date: false, .. } = d {
        return Err(report(DtErr::BadFormat, input, type_name, ""));
    }
    Ok(d)
}

/// `DecodeTimeOnly` for `time` / `timetz` input.
pub fn decode_time(input: &str, type_name: &str) -> SqlResult<Decoded> {
    let st = decode(input, true).map_err(|(e, z)| report(e, input, type_name, &z))?;
    if st.special.is_some() {
        return Err(report(DtErr::BadFormat, input, type_name, ""));
    }
    let has_date = st.fmask & DATE_M != 0;
    if has_date && st.fmask & DATE_M != DATE_M {
        return Err(report(DtErr::BadFormat, input, type_name, ""));
    }
    let d = finish(st);
    if let Decoded::Fields { has_time: false, .. } = d {
        return Err(report(DtErr::BadFormat, input, type_name, ""));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(s: &str) -> Vec<(String, Ftype)> {
        split_fields(s).expect("split")
    }

    #[test]
    fn splits_like_parse_datetime() {
        assert_eq!(fields("2024-03-15 10:00:00-05").iter().map(|f| f.1).collect::<Vec<_>>(), [Ftype::Date, Ftype::Time, Ftype::Tz]);
        assert_eq!(fields("15 March 2024")[1], ("march".into(), Ftype::Str));
        assert_eq!(fields("2024-03-15 10:00 America/New_York")[2], ("america/new_york".into(), Ftype::Date));
        assert_eq!(fields("2024-03-15T10:00")[1], ("t".into(), Ftype::Str));
    }

    #[test]
    fn decodes_dates_in_date_order() {
        let ymd = |s: &str| match decode_datetime(s, "date").expect(s) {
            Decoded::Fields { tm, .. } => (tm.year, tm.month, tm.day),
            other => panic!("{other:?}"),
        };
        assert_eq!(ymd("1/2/2024"), (2024, 1, 2));
        assert_eq!(ymd("February 29, 2024"), (2024, 2, 29));
        assert_eq!(ymd("15-mar-24"), (2024, 3, 15));
        assert_eq!(ymd("20240315"), (2024, 3, 15));
        assert_eq!(ymd("2024.075"), (2024, 3, 15));
        assert_eq!(ymd("J2451545"), (2000, 1, 1));
        assert!(decode_datetime("2023-02-29", "date").is_err());
    }
}
