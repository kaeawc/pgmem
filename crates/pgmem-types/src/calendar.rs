//! Calendar arithmetic, ported from Postgres (`src/backend/utils/adt/datetime.c`,
//! `timestamp.c`): Julian day numbers, broken-down times and ISO weeks. Years are
//! astronomical (year 0 is 1 BC), as in Postgres' internal representation.

/// Julian day number of 2000-01-01, the Postgres epoch.
pub const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
pub const USECS_PER_DAY: i64 = 86_400_000_000;

/// `date2j`: Julian day number of a proleptic Gregorian date.
pub fn date2j(year: i32, month: i32, day: i32) -> i64 {
    let (mut y, mut m) = (i64::from(year), i64::from(month));
    if m > 2 {
        m += 1;
        y += 4800;
    } else {
        m += 13;
        y += 4799;
    }
    let century = y.div_euclid(100);
    let mut julian = y * 365 - 32167;
    julian += y.div_euclid(4) - century + century.div_euclid(4);
    julian += 7834 * m / 256 + i64::from(day);
    julian
}

/// `j2date`: the date of a Julian day number.
pub fn j2date(jd: i64) -> (i32, u32, u32) {
    let mut julian = jd + 32044;
    let mut quad = julian.div_euclid(146_097);
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian += 60 + quad * 3 + extra.div_euclid(146_097);
    quad = julian.div_euclid(1461);
    julian -= quad * 1461;
    let mut y = julian * 4 / 1461;
    julian = if y != 0 { (julian + 305) % 365 } else { (julian + 306) % 366 } + 123;
    y += quad * 4;
    let year = y - 4800;
    let q = julian * 2141 / 65536;
    let day = julian - 7834 * q / 256;
    let month = (q + 10) % 12 + 1;
    (i32::try_from(year).unwrap_or(i32::MAX), u32::try_from(month).unwrap_or(1), u32::try_from(day).unwrap_or(1))
}

/// `j2day`: day of week of a Julian day, 0 = Sunday.
pub fn j2day(jd: i64) -> u32 {
    u32::try_from((jd + 1).rem_euclid(7)).unwrap_or(0)
}

pub fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
    }
}

/// True if the date exists.
pub fn valid_date(year: i32, month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// Days since 2000-01-01 for a date.
pub fn days_from_ymd(year: i32, month: u32, day: u32) -> i64 {
    date2j(year, i32::try_from(month).unwrap_or(1), i32::try_from(day).unwrap_or(1)) - POSTGRES_EPOCH_JDATE
}

/// The date of a day count since 2000-01-01.
pub fn ymd_from_days(days: i64) -> (i32, u32, u32) {
    j2date(days + POSTGRES_EPOCH_JDATE)
}

/// A broken-down UTC time (Postgres' `struct pg_tm` plus microseconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micros: u32,
}

impl Tm {
    /// From microseconds since the Postgres epoch.
    pub fn from_micros(t: i64) -> Tm {
        let days = t.div_euclid(USECS_PER_DAY);
        let mut rem = t.rem_euclid(USECS_PER_DAY);
        let (year, month, day) = ymd_from_days(days);
        let hour = rem / 3_600_000_000;
        rem %= 3_600_000_000;
        let minute = rem / 60_000_000;
        rem %= 60_000_000;
        let second = rem / 1_000_000;
        let micros = rem % 1_000_000;
        let u = |v: i64| u32::try_from(v).unwrap_or(0);
        Tm { year, month, day, hour: u(hour), minute: u(minute), second: u(second), micros: u(micros) }
    }

    /// To microseconds since the Postgres epoch (no validation).
    pub fn to_micros(&self) -> i64 {
        days_from_ymd(self.year, self.month, self.day) * USECS_PER_DAY
            + i64::from(self.hour) * 3_600_000_000
            + i64::from(self.minute) * 60_000_000
            + i64::from(self.second) * 1_000_000
            + i64::from(self.micros)
    }

    pub fn julian(&self) -> i64 {
        date2j(self.year, i32::try_from(self.month).unwrap_or(1), i32::try_from(self.day).unwrap_or(1))
    }

    /// Day of week, 0 = Sunday.
    pub fn dow(&self) -> u32 {
        j2day(self.julian())
    }

    /// ISO day of week, 1 = Monday .. 7 = Sunday.
    pub fn isodow(&self) -> u32 {
        match self.dow() {
            0 => 7,
            d => d,
        }
    }

    /// Day of year, 1-based.
    pub fn doy(&self) -> u32 {
        u32::try_from(self.julian() - date2j(self.year, 1, 1) + 1).unwrap_or(1)
    }

    /// (ISO year, ISO week), Postgres' `date2isoweek` / `date2isoyear`.
    pub fn iso_week(&self) -> (i32, u32) {
        let dayn = self.julian();
        let mut year = self.year;
        let mut day4 = date2j(year, 1, 4);
        let mut day0 = i64::from(j2day(day4 - 1));
        if dayn < day4 - day0 {
            year -= 1;
            day4 = date2j(year, 1, 4);
            day0 = i64::from(j2day(day4 - 1));
        }
        let mut week = (dayn - (day4 - day0)).div_euclid(7) + 1;
        if week >= 52 {
            let next4 = date2j(year + 1, 1, 4);
            let next0 = i64::from(j2day(next4 - 1));
            if dayn >= next4 - next0 {
                year += 1;
                week = (dayn - (next4 - next0)).div_euclid(7) + 1;
            }
        }
        (year, u32::try_from(week).unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn julian_round_trip() {
        assert_eq!(date2j(2000, 1, 1), POSTGRES_EPOCH_JDATE);
        for days in [-1_000_000i64, -730_000, -1, 0, 1, 59, 60, 366, 10_000, 3_000_000] {
            let (y, m, d) = ymd_from_days(days);
            assert!(valid_date(y, m, d), "{days} -> {y}-{m}-{d}");
            assert_eq!(days_from_ymd(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(ymd_from_days(59), (2000, 2, 29));
        assert_eq!(ymd_from_days(-1), (1999, 12, 31));
    }

    #[test]
    fn weekdays_and_iso_weeks() {
        let tm = Tm::from_micros(0);
        assert_eq!(tm.dow(), 6, "2000-01-01 was a Saturday");
        let t = |y, m, d| Tm { year: y, month: m, day: d, hour: 0, minute: 0, second: 0, micros: 0 };
        assert_eq!(t(2021, 1, 1).iso_week(), (2020, 53));
        assert_eq!(t(2024, 12, 30).iso_week(), (2025, 1));
        assert_eq!(t(2024, 7, 15).iso_week(), (2024, 29));
        assert_eq!(t(2024, 12, 31).doy(), 366);
    }

    #[test]
    fn micros_round_trip() {
        for t in [0i64, -1, 1, 86_399_999_999, -86_400_000_001, 1_234_567_890_123_456] {
            assert_eq!(Tm::from_micros(t).to_micros(), t);
        }
    }
}
