//! Calendar functions with SQL Server semantics.
//!
//! `DATEPART`, `DATENAME`, `DATEADD`, `DATEDIFF`, `DATEDIFF_BIG`, `EOMONTH`,
//! `DATETRUNC`, `DATE_BUCKET` and `CONVERT` with a style number are computed
//! here instead of being approximated with `strftime`. Notable behaviours:
//!
//! * `DATEDIFF` counts *boundaries crossed* (`DATEDIFF(day, '23:00 Mon',
//!   '01:00 Tue')` is 1), not elapsed time.
//! * `DATEADD(month, 1, '2026-01-31')` clamps to the end of the shorter month.
//! * Weeks follow SQL Server's default `DATEFIRST 7` (weeks start on Sunday,
//!   `weekday` 1 = Sunday); `iso_week` is ISO 8601.
//! * Unknown date parts, unparsable dates and unsupported `CONVERT` styles
//!   raise an error. They never produce `NULL` or a guessed value.
//!
//! Time zones are not modelled: offsets in the input text are ignored and
//! results carry no offset (documented limitation).

use rusqlite::{
    Connection,
    functions::{Context, FunctionFlags},
    types::Value,
};

use super::tsql_functions::{DateTime, days_in_month, parse_datetime, value_text};
use crate::Result;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;

type FnResult<T> = rusqlite::Result<T>;
type StrResult<T> = std::result::Result<T, String>;

fn error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into().into())
}

// ---------------------------------------------------------------------------
// Civil calendar arithmetic (proleptic Gregorian, days since 1970-01-01)
// ---------------------------------------------------------------------------

pub(super) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn day_of_year(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day) - days_from_civil(year, 1, 1) + 1
}

/// Sunday = 0.
fn weekday_sunday0(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn iso_week(year: i64, month: u32, day: u32) -> i64 {
    let days = days_from_civil(year, month, day);
    let weekday = (days + 3).rem_euclid(7) + 1; // Monday = 1
    let ordinal = day_of_year(year, month, day);
    let week = (ordinal - weekday + 10) / 7;
    let weeks_in = |y: i64| {
        let jan1 = (days_from_civil(y, 1, 1) + 3).rem_euclid(7) + 1;
        let leap = days_in_month(y, 2) == 29;
        if jan1 == 4 || (leap && jan1 == 3) {
            53
        } else {
            52
        }
    };
    if week < 1 {
        weeks_in(year - 1)
    } else if week > weeks_in(year) {
        1
    } else {
        week
    }
}

// ---------------------------------------------------------------------------
// Instants
// ---------------------------------------------------------------------------

/// A calendar value plus whether it carried a time of day.
#[derive(Clone, Copy)]
struct Stamp {
    /// Nanoseconds since 1970-01-01T00:00:00.
    nanos: i128,
    has_time: bool,
}

impl Stamp {
    fn from_datetime(dt: &DateTime) -> Self {
        let days = days_from_civil(dt.year, dt.month, dt.day);
        let time = i128::from(dt.hour) * 3_600 * NANOS_PER_SECOND
            + i128::from(dt.minute) * 60 * NANOS_PER_SECOND
            + i128::from(dt.second) * NANOS_PER_SECOND
            + i128::from(dt.nanos);
        Self {
            nanos: i128::from(days) * NANOS_PER_DAY + time,
            has_time: dt.has_time,
        }
    }

    fn days(&self) -> i64 {
        self.nanos.div_euclid(NANOS_PER_DAY) as i64
    }

    fn time_of_day(&self) -> i64 {
        self.nanos.rem_euclid(NANOS_PER_DAY) as i64
    }

    fn to_datetime(self) -> DateTime {
        let (year, month, day) = civil_from_days(self.days());
        let tod = self.time_of_day();
        let seconds = tod / 1_000_000_000;
        DateTime {
            year,
            month,
            day,
            hour: (seconds / 3_600) as u32,
            minute: ((seconds % 3_600) / 60) as u32,
            second: (seconds % 60) as u32,
            nanos: (tod % 1_000_000_000) as u32,
            has_time: self.has_time,
        }
    }

    /// Renders the value; date-only values stay date-only.
    fn render(self) -> String {
        let dt = self.to_datetime();
        if self.has_time {
            dt.datetime_string()
        } else {
            dt.date_string()
        }
    }
}

/// Days since 1900-01-01 (SQL Server's integer-to-datetime convention).
const EPOCH_1900: i64 = -25_567;

fn stamp_from_value(value: &Value) -> FnResult<Option<Stamp>> {
    match value {
        Value::Null => Ok(None),
        Value::Integer(days) => Ok(Some(Stamp {
            nanos: i128::from(days + EPOCH_1900) * NANOS_PER_DAY,
            has_time: false,
        })),
        Value::Real(days) => {
            let whole = days.floor();
            let fraction = ((days - whole) * 86_400.0 * 1e9).round() as i128;
            Ok(Some(Stamp {
                nanos: i128::from(whole as i64 + EPOCH_1900) * NANOS_PER_DAY + fraction,
                has_time: true,
            }))
        }
        Value::Text(_) | Value::Blob(_) => {
            let text = value_text(value);
            match parse_datetime(&text) {
                Some(dt) => Ok(Some(Stamp::from_datetime(&dt))),
                None => Err(error(format!(
                    "Conversion failed when converting date and/or time from character string '{text}'."
                ))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Date parts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Year,
    Quarter,
    Month,
    DayOfYear,
    Day,
    Week,
    Weekday,
    IsoWeek,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
    TzOffset,
}

impl Part {
    fn parse(name: &str) -> FnResult<Self> {
        let key = name.trim().trim_matches('\'').to_ascii_lowercase();
        Ok(match key.as_str() {
            "year" | "yy" | "yyyy" => Self::Year,
            "quarter" | "qq" | "q" => Self::Quarter,
            "month" | "mm" | "m" => Self::Month,
            "dayofyear" | "dy" | "y" => Self::DayOfYear,
            "day" | "dd" | "d" => Self::Day,
            "week" | "wk" | "ww" => Self::Week,
            "weekday" | "dw" | "w" => Self::Weekday,
            "iso_week" | "isowk" | "isoww" => Self::IsoWeek,
            "hour" | "hh" => Self::Hour,
            "minute" | "mi" | "n" => Self::Minute,
            "second" | "ss" | "s" => Self::Second,
            "millisecond" | "ms" => Self::Millisecond,
            "microsecond" | "mcs" => Self::Microsecond,
            "nanosecond" | "ns" => Self::Nanosecond,
            "tzoffset" | "tz" => Self::TzOffset,
            _ => {
                return Err(error(format!(
                    "'{name}' is not a recognized date part option."
                )));
            }
        })
    }

    /// Length in nanoseconds for parts that are a fixed duration.
    fn fixed_nanos(self) -> Option<i128> {
        Some(match self {
            Self::Day | Self::DayOfYear | Self::Weekday => NANOS_PER_DAY,
            Self::Week | Self::IsoWeek => 7 * NANOS_PER_DAY,
            Self::Hour => 3_600 * NANOS_PER_SECOND,
            Self::Minute => 60 * NANOS_PER_SECOND,
            Self::Second => NANOS_PER_SECOND,
            Self::Millisecond => 1_000_000,
            Self::Microsecond => 1_000,
            Self::Nanosecond => 1,
            _ => return None,
        })
    }

    fn is_time_part(self) -> bool {
        matches!(
            self,
            Self::Hour
                | Self::Minute
                | Self::Second
                | Self::Millisecond
                | Self::Microsecond
                | Self::Nanosecond
        )
    }
}

fn date_part_value(stamp: Stamp, part: Part) -> i64 {
    let dt = stamp.to_datetime();
    let days = stamp.days();
    match part {
        Part::Year => dt.year,
        Part::Quarter => i64::from((dt.month - 1) / 3 + 1),
        Part::Month => i64::from(dt.month),
        Part::DayOfYear => day_of_year(dt.year, dt.month, dt.day),
        Part::Day => i64::from(dt.day),
        Part::Week => {
            let jan1 = weekday_sunday0(days_from_civil(dt.year, 1, 1));
            (day_of_year(dt.year, dt.month, dt.day) - 1 + jan1) / 7 + 1
        }
        Part::Weekday => weekday_sunday0(days) + 1,
        Part::IsoWeek => iso_week(dt.year, dt.month, dt.day),
        Part::Hour => i64::from(dt.hour),
        Part::Minute => i64::from(dt.minute),
        Part::Second => i64::from(dt.second),
        Part::Millisecond => i64::from(dt.nanos / 1_000_000),
        Part::Microsecond => i64::from(dt.nanos / 1_000),
        Part::Nanosecond => i64::from(dt.nanos),
        Part::TzOffset => 0,
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn datepart_fn(ctx: &Context<'_>) -> FnResult<Option<i64>> {
    let part = Part::parse(&ctx.get::<String>(0)?)?;
    let Some(stamp) = stamp_from_value(&ctx.get::<Value>(1)?)? else {
        return Ok(None);
    };
    Ok(Some(date_part_value(stamp, part)))
}

fn datename_fn(ctx: &Context<'_>) -> FnResult<Option<String>> {
    let part = Part::parse(&ctx.get::<String>(0)?)?;
    let Some(stamp) = stamp_from_value(&ctx.get::<Value>(1)?)? else {
        return Ok(None);
    };
    let dt = stamp.to_datetime();
    Ok(Some(match part {
        Part::Month => MONTHS[(dt.month - 1) as usize].to_owned(),
        Part::Weekday => WEEKDAYS[weekday_sunday0(stamp.days()) as usize].to_owned(),
        Part::TzOffset => "+00:00".to_owned(),
        other => date_part_value(stamp, other).to_string(),
    }))
}

// ---------------------------------------------------------------------------
// DATEADD / DATEDIFF
// ---------------------------------------------------------------------------

fn add_months(stamp: Stamp, months: i64) -> FnResult<Stamp> {
    let dt = stamp.to_datetime();
    let total = dt.year * 12 + i64::from(dt.month - 1) + months;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) as u32 + 1;
    if !(1..=9999).contains(&year) {
        return Err(error(
            "Adding a value to a 'datetime' column caused an overflow.",
        ));
    }
    let day = dt.day.min(days_in_month(year, month));
    let days = days_from_civil(year, month, day);
    Ok(Stamp {
        nanos: i128::from(days) * NANOS_PER_DAY + i128::from(stamp.time_of_day()),
        has_time: stamp.has_time,
    })
}

fn number_arg(ctx: &Context<'_>, index: usize) -> FnResult<Option<i64>> {
    match ctx.get::<Value>(index)? {
        Value::Null => Ok(None),
        Value::Integer(n) => Ok(Some(n)),
        Value::Real(r) if r.is_finite() => Ok(Some(r.trunc() as i64)),
        Value::Text(text) => text.trim().parse::<i64>().map(Some).map_err(|_| {
            error(format!(
                "Error converting data type varchar to int: '{text}'."
            ))
        }),
        _ => Err(error("DATEADD: the number argument must be numeric.")),
    }
}

pub(super) fn dateadd(part: &str, amount: i64, value: &Value) -> FnResult<Option<String>> {
    let part = Part::parse(part)?;
    let Some(stamp) = stamp_from_value(value)? else {
        return Ok(None);
    };
    let result = match part {
        Part::Year => add_months(stamp, amount.saturating_mul(12))?,
        Part::Quarter => add_months(stamp, amount.saturating_mul(3))?,
        Part::Month => add_months(stamp, amount)?,
        Part::TzOffset => return Err(error("DATEADD does not support the tzoffset date part.")),
        other => {
            let unit = other.fixed_nanos().unwrap_or(NANOS_PER_DAY);
            Stamp {
                nanos: stamp.nanos + i128::from(amount) * unit,
                has_time: stamp.has_time || other.is_time_part(),
            }
        }
    };
    let year = civil_from_days(result.days()).0;
    if !(1..=9999).contains(&year) {
        return Err(error(
            "Adding a value to a 'datetime' column caused an overflow.",
        ));
    }
    Ok(Some(result.render()))
}

/// The ordinal of the calendar bucket of `part` containing `stamp`; the
/// difference of two ordinals is what `DATEDIFF` returns.
fn boundary_ordinal(stamp: Stamp, part: Part) -> FnResult<i128> {
    let dt = stamp.to_datetime();
    Ok(match part {
        Part::Year => i128::from(dt.year),
        Part::Quarter => i128::from(dt.year * 4 + i64::from((dt.month - 1) / 3)),
        Part::Month => i128::from(dt.year * 12 + i64::from(dt.month - 1)),
        Part::Week => i128::from((stamp.days() + 4).div_euclid(7)),
        Part::IsoWeek => i128::from((stamp.days() + 3).div_euclid(7)),
        Part::TzOffset => return Err(error("DATEDIFF does not support the tzoffset date part.")),
        other => stamp
            .nanos
            .div_euclid(other.fixed_nanos().unwrap_or(NANOS_PER_DAY)),
    })
}

fn datediff_value(part: &str, start: &Value, end: &Value) -> FnResult<Option<i128>> {
    let part = Part::parse(part)?;
    let (Some(start), Some(end)) = (stamp_from_value(start)?, stamp_from_value(end)?) else {
        return Ok(None);
    };
    Ok(Some(
        boundary_ordinal(end, part)? - boundary_ordinal(start, part)?,
    ))
}

fn datediff_fn(ctx: &Context<'_>) -> FnResult<Option<i64>> {
    let part: String = ctx.get(0)?;
    let Some(diff) = datediff_value(&part, &ctx.get::<Value>(1)?, &ctx.get::<Value>(2)?)? else {
        return Ok(None);
    };
    i32::try_from(diff).map(|value| Some(i64::from(value))).map_err(|_| {
        error(format!(
            "The datediff function resulted in an overflow. The number of dateparts separating two date/time instances is too large. Try to use datediff with a less precise datepart ({part})."
        ))
    })
}

fn datediff_big_fn(ctx: &Context<'_>) -> FnResult<Option<i64>> {
    let part: String = ctx.get(0)?;
    let Some(diff) = datediff_value(&part, &ctx.get::<Value>(1)?, &ctx.get::<Value>(2)?)? else {
        return Ok(None);
    };
    i64::try_from(diff)
        .map(Some)
        .map_err(|_| error("The datediff_big function resulted in an overflow."))
}

// ---------------------------------------------------------------------------
// EOMONTH / DATETRUNC / DATE_BUCKET
// ---------------------------------------------------------------------------

fn eomonth_fn(ctx: &Context<'_>) -> FnResult<Option<String>> {
    let Some(stamp) = stamp_from_value(&ctx.get::<Value>(0)?)? else {
        return Ok(None);
    };
    let months = if ctx.len() > 1 {
        number_arg(ctx, 1)?.unwrap_or_default()
    } else {
        0
    };
    let shifted = add_months(stamp, months)?.to_datetime();
    let last = days_in_month(shifted.year, shifted.month);
    Ok(Some(format!(
        "{:04}-{:02}-{:02}",
        shifted.year, shifted.month, last
    )))
}

fn truncate(stamp: Stamp, part: Part) -> FnResult<Stamp> {
    let dt = stamp.to_datetime();
    let from_days = |days: i64, has_time: bool| Stamp {
        nanos: i128::from(days) * NANOS_PER_DAY,
        has_time,
    };
    Ok(match part {
        Part::Year => from_days(days_from_civil(dt.year, 1, 1), stamp.has_time),
        Part::Quarter => from_days(
            days_from_civil(dt.year, (dt.month - 1) / 3 * 3 + 1, 1),
            stamp.has_time,
        ),
        Part::Month => from_days(days_from_civil(dt.year, dt.month, 1), stamp.has_time),
        Part::Week => from_days(stamp.days() - weekday_sunday0(stamp.days()), stamp.has_time),
        Part::IsoWeek => from_days(
            stamp.days() - (stamp.days() + 3).rem_euclid(7),
            stamp.has_time,
        ),
        Part::Day | Part::DayOfYear | Part::Weekday => from_days(stamp.days(), stamp.has_time),
        Part::Hour | Part::Minute | Part::Second | Part::Millisecond | Part::Microsecond => {
            let unit = part.fixed_nanos().unwrap_or(1);
            Stamp {
                nanos: stamp.nanos.div_euclid(unit) * unit,
                has_time: true,
            }
        }
        Part::Nanosecond | Part::TzOffset => {
            return Err(error("DATETRUNC does not support this date part."));
        }
    })
}

fn datetrunc_fn(ctx: &Context<'_>) -> FnResult<Option<String>> {
    let part = Part::parse(&ctx.get::<String>(0)?)?;
    let Some(stamp) = stamp_from_value(&ctx.get::<Value>(1)?)? else {
        return Ok(None);
    };
    Ok(Some(truncate(stamp, part)?.render()))
}

fn date_bucket_fn(ctx: &Context<'_>) -> FnResult<Option<String>> {
    let part = Part::parse(&ctx.get::<String>(0)?)?;
    let Some(width) = number_arg(ctx, 1)? else {
        return Ok(None);
    };
    if width <= 0 {
        return Err(error(
            "Invalid bucket width value passed to date_bucket function. Only positive values are allowed.",
        ));
    }
    let Some(stamp) = stamp_from_value(&ctx.get::<Value>(2)?)? else {
        return Ok(None);
    };
    let origin = if ctx.len() > 3 {
        match stamp_from_value(&ctx.get::<Value>(3)?)? {
            Some(origin) => origin,
            None => return Ok(None),
        }
    } else {
        // 1900-01-01, a Monday, like SQL Server.
        Stamp {
            nanos: i128::from(EPOCH_1900) * NANOS_PER_DAY,
            has_time: false,
        }
    };
    let has_time = stamp.has_time || part.is_time_part();
    let bucket = match part {
        Part::Year | Part::Quarter | Part::Month => {
            let per = match part {
                Part::Year => 12,
                Part::Quarter => 3,
                _ => 1,
            };
            let months = |s: Stamp| {
                let dt = s.to_datetime();
                dt.year * 12 + i64::from(dt.month - 1)
            };
            let step = width.saturating_mul(per);
            let offset = (months(stamp) - months(origin)).div_euclid(step) * step;
            let target = months(origin) + offset;
            let (year, month) = (target.div_euclid(12), target.rem_euclid(12) as u32 + 1);
            Stamp {
                nanos: i128::from(days_from_civil(year, month, 1)) * NANOS_PER_DAY,
                has_time,
            }
        }
        other => {
            let unit = other
                .fixed_nanos()
                .ok_or_else(|| error("DATE_BUCKET does not support this date part."))?
                * i128::from(width);
            let offset = (stamp.nanos - origin.nanos).div_euclid(unit) * unit;
            Stamp {
                nanos: origin.nanos + offset,
                has_time,
            }
        }
    };
    Ok(Some(bucket.render()))
}

// ---------------------------------------------------------------------------
// CONVERT with a style number
// ---------------------------------------------------------------------------

fn pad2(value: u32) -> String {
    format!("{value:02}")
}

fn clock12(dt: &DateTime) -> (u32, &'static str) {
    let hour = match dt.hour % 12 {
        0 => 12,
        other => other,
    };
    (hour, if dt.hour < 12 { "AM" } else { "PM" })
}

/// Formats `dt` the way `CONVERT(varchar, dt, style)` does.
fn format_with_style(dt: &DateTime, style: i64) -> StrResult<String> {
    let key = match style {
        0 | 100 => 0,
        1..=14 | 20..=25 | 120 | 121 | 126 | 127 => style,
        101..=114 => style - 100,
        _ => return Err(format!("CONVERT style {style} is not supported.")),
    };
    let mon = MONTHS_SHORT[(dt.month - 1) as usize];
    let (hour12, meridiem) = clock12(dt);
    let ms = dt.nanos / 1_000_000;
    let hms = format!("{}:{}:{}", pad2(dt.hour), pad2(dt.minute), pad2(dt.second));
    let text = match key {
        0 => format!(
            "{mon} {:>2} {} {:>2}:{}{meridiem}",
            dt.day,
            dt.year,
            hour12,
            pad2(dt.minute)
        ),
        1 => format!("{}/{}/{:02}", pad2(dt.month), pad2(dt.day), dt.year % 100),
        2 => format!("{:02}.{}.{}", dt.year % 100, pad2(dt.month), pad2(dt.day)),
        3 => format!("{}/{}/{:02}", pad2(dt.day), pad2(dt.month), dt.year % 100),
        4 => format!("{}.{}.{:02}", pad2(dt.day), pad2(dt.month), dt.year % 100),
        5 => format!("{}-{}-{:02}", pad2(dt.day), pad2(dt.month), dt.year % 100),
        6 => format!("{} {mon} {:02}", pad2(dt.day), dt.year % 100),
        7 => format!("{mon} {}, {:02}", pad2(dt.day), dt.year % 100),
        8 => hms,
        9 => format!(
            "{mon} {:>2} {} {:>2}:{}:{}:{ms:03}{meridiem}",
            dt.day,
            dt.year,
            hour12,
            pad2(dt.minute),
            pad2(dt.second)
        ),
        10 => format!("{}-{}-{:02}", pad2(dt.month), pad2(dt.day), dt.year % 100),
        11 => format!("{:02}/{}/{}", dt.year % 100, pad2(dt.month), pad2(dt.day)),
        12 => format!("{:02}{}{}", dt.year % 100, pad2(dt.month), pad2(dt.day)),
        13 => format!("{} {mon} {} {hms}:{ms:03}", pad2(dt.day), dt.year),
        14 => format!("{hms}:{ms:03}"),
        20 => format!("{:04}-{}-{} {hms}", dt.year, pad2(dt.month), pad2(dt.day)),
        21 | 25 => format!(
            "{:04}-{}-{} {hms}.{ms:03}",
            dt.year,
            pad2(dt.month),
            pad2(dt.day)
        ),
        22 => format!(
            "{}/{}/{:02} {:>2}:{}:{} {meridiem}",
            pad2(dt.month),
            pad2(dt.day),
            dt.year % 100,
            hour12,
            pad2(dt.minute),
            pad2(dt.second)
        ),
        23 => format!("{:04}-{}-{}", dt.year, pad2(dt.month), pad2(dt.day)),
        24 => hms,
        120 => format!("{:04}-{}-{} {hms}", dt.year, pad2(dt.month), pad2(dt.day)),
        121 | 125 => format!(
            "{:04}-{}-{} {hms}.{ms:03}",
            dt.year,
            pad2(dt.month),
            pad2(dt.day)
        ),
        126 => format!(
            "{:04}-{}-{}T{hms}.{ms:03}",
            dt.year,
            pad2(dt.month),
            pad2(dt.day)
        ),
        127 => format!(
            "{:04}-{}-{}T{hms}.{ms:03}Z",
            dt.year,
            pad2(dt.month),
            pad2(dt.day)
        ),
        _ => return Err(format!("CONVERT style {style} is not supported.")),
    };
    // Styles 101-112 add the century; the short forms above use two digits.
    if (101..=112).contains(&style) {
        return Ok(match key {
            1 => format!("{}/{}/{}", pad2(dt.month), pad2(dt.day), dt.year),
            2 => format!("{}.{}.{}", dt.year, pad2(dt.month), pad2(dt.day)),
            3 => format!("{}/{}/{}", pad2(dt.day), pad2(dt.month), dt.year),
            4 => format!("{}.{}.{}", pad2(dt.day), pad2(dt.month), dt.year),
            5 => format!("{}-{}-{}", pad2(dt.day), pad2(dt.month), dt.year),
            6 => format!("{} {mon} {}", pad2(dt.day), dt.year),
            7 => format!("{mon} {}, {}", pad2(dt.day), dt.year),
            10 => format!("{}-{}-{}", pad2(dt.month), pad2(dt.day), dt.year),
            11 => format!("{}/{}/{}", dt.year, pad2(dt.month), pad2(dt.day)),
            12 => format!("{}{}{}", dt.year, pad2(dt.month), pad2(dt.day)),
            _ => text,
        });
    }
    Ok(text)
}

/// Parses `text` written in a style-specific layout (`CONVERT(DATE, x, style)`).
fn parse_with_style(text: &str, style: i64) -> StrResult<DateTime> {
    let trimmed = text.trim();
    let (date_part, rest) = match trimmed.split_once(' ') {
        Some((date, rest)) => (date, Some(rest.trim())),
        None => (trimmed, None),
    };
    let fields = |separator: char| -> Option<Vec<u32>> {
        let parts: Vec<&str> = date_part.split(separator).collect();
        if parts.len() != 3 {
            return None;
        }
        parts.iter().map(|part| part.parse::<u32>().ok()).collect()
    };
    let year4 = |value: u32| -> i64 {
        if value < 100 {
            i64::from(if value <= 49 {
                2000 + value
            } else {
                1900 + value
            })
        } else {
            i64::from(value)
        }
    };
    let ordered = match style {
        1 | 101 => fields('/').map(|f| (year4(f[2]), f[0], f[1])),
        2 | 102 => fields('.').map(|f| (year4(f[0]), f[1], f[2])),
        3 | 103 => fields('/').map(|f| (year4(f[2]), f[1], f[0])),
        4 | 104 => fields('.').map(|f| (year4(f[2]), f[1], f[0])),
        5 | 105 => fields('-').map(|f| (year4(f[2]), f[1], f[0])),
        10 | 110 => fields('-').map(|f| (year4(f[2]), f[0], f[1])),
        11 | 111 => fields('/').map(|f| (year4(f[0]), f[1], f[2])),
        0 | 12 | 20 | 21 | 23 | 25 | 112 | 120 | 121 | 126 | 127 | 100 => {
            return parse_datetime(trimmed).ok_or_else(|| {
                "Conversion failed when converting date and/or time from character string."
                    .to_owned()
            });
        }
        _ => {
            return Err(format!(
                "CONVERT style {style} is not supported for parsing dates."
            ));
        }
    };
    let (year, month, day) = ordered.ok_or_else(|| {
        "Conversion failed when converting date and/or time from character string.".to_owned()
    })?;
    let iso = format!("{year:04}-{month:02}-{day:02}");
    let combined = match rest {
        Some(rest) if !rest.is_empty() => format!("{iso} {rest}"),
        _ => iso,
    };
    parse_datetime(&combined).ok_or_else(|| {
        "Conversion failed when converting date and/or time from character string.".to_owned()
    })
}

fn money_style(value: f64, style: i64) -> StrResult<String> {
    match style {
        0 => Ok(format!("{value}")),
        1 => {
            let fixed = format!("{:.2}", value.abs());
            let (whole, fraction) = fixed.split_once('.').unwrap_or((&fixed, "00"));
            let mut grouped = String::new();
            for (index, digit) in whole.chars().enumerate() {
                if index > 0 && (whole.len() - index) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(digit);
            }
            Ok(format!(
                "{}{grouped}.{fraction}",
                if value < 0.0 { "-" } else { "" }
            ))
        }
        2 => Ok(format!("{value:.4}")),
        other => Err(format!(
            "CONVERT style {other} is not supported for numeric values."
        )),
    }
}

/// Backs `CONVERT(type, value, style)` and `CAST(value AS <date type>)`.
///
/// `type_name` is the target type as written (`'varchar(10)'`, `'DATE'`, ...).
fn convert_fn(ctx: &Context<'_>) -> FnResult<Value> {
    let type_name: String = ctx.get(0)?;
    let value: Value = ctx.get(1)?;
    let style: Option<i64> = if ctx.len() > 2 { ctx.get(2)? } else { None };
    convert_value(&type_name, value, style).map_err(error)
}

pub(super) fn convert_value(type_name: &str, value: Value, style: Option<i64>) -> StrResult<Value> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let lower = type_name.trim().to_ascii_lowercase();
    let (base, args) = match lower.split_once('(') {
        Some((base, rest)) => (
            base.trim().to_owned(),
            rest.trim_end_matches(')').trim().to_owned(),
        ),
        None => (lower.clone(), String::new()),
    };
    let style_number = style.unwrap_or(0);
    let parse_input = |value: &Value| -> StrResult<DateTime> {
        if let Value::Text(text) = value {
            parse_with_style(text, style_number)
        } else {
            stamp_from_value(value)
                .map_err(|e| e.to_string())?
                .map(Stamp::to_datetime)
                .ok_or_else(|| "Conversion failed.".to_owned())
        }
    };
    match base.as_str() {
        "date" | "datetime" | "datetime2" | "smalldatetime" | "time" | "datetimeoffset" => {
            let dt = parse_input(&value)?;
            Ok(Value::Text(match base.as_str() {
                "date" => dt.date_string(),
                "time" => {
                    let mut time = dt;
                    time.has_time = true;
                    time.time_string()
                }
                "smalldatetime" => {
                    let rounded = if dt.second >= 30 {
                        let stamp = Stamp::from_datetime(&dt);
                        let minute = stamp.nanos.div_euclid(60 * NANOS_PER_SECOND) + 1;
                        Stamp {
                            nanos: minute * 60 * NANOS_PER_SECOND,
                            has_time: true,
                        }
                        .to_datetime()
                    } else {
                        dt
                    };
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:00",
                        rounded.year, rounded.month, rounded.day, rounded.hour, rounded.minute
                    )
                }
                "datetime" => format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                    dt.year,
                    dt.month,
                    dt.day,
                    dt.hour,
                    dt.minute,
                    dt.second,
                    dt.nanos / 1_000_000
                ),
                "datetimeoffset" => format!("{} +00:00", {
                    let mut with_time = dt;
                    with_time.has_time = true;
                    with_time.datetime_string()
                }),
                _ => {
                    let mut with_time = dt;
                    with_time.has_time = true;
                    with_time.datetime_string()
                }
            }))
        }
        "varchar" | "nvarchar" | "char" | "nchar" | "text" | "ntext" => {
            let limit = args.parse::<usize>().ok();
            let text = match (&value, style) {
                (Value::Real(real), Some(style)) => money_style(*real, style)?,
                (Value::Integer(int), Some(style)) => money_style(*int as f64, style)?,
                (Value::Text(text), Some(style)) if style != 0 => match parse_datetime(text) {
                    Some(dt) => format_with_style(&dt, style)?,
                    None => {
                        return Err(format!(
                            "CONVERT style {style} needs a date/time value, got '{text}'."
                        ));
                    }
                },
                (Value::Text(text), _) => text.clone(),
                (other, _) => value_text(other),
            };
            Ok(Value::Text(match limit {
                Some(limit) => text.chars().take(limit).collect(),
                None => text,
            }))
        }
        other => {
            if style_number != 0 {
                Err(format!(
                    "CONVERT style {style_number} is not supported for target type `{other}`."
                ))
            } else {
                Err(format!("CONVERT: unsupported target type `{other}`."))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(connection: &Connection) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    connection.create_scalar_function("datepart", 2, deterministic, datepart_fn)?;
    connection.create_scalar_function("datename", 2, deterministic, datename_fn)?;
    connection.create_scalar_function("dateadd", 3, deterministic, |ctx| {
        let part: String = ctx.get(0)?;
        let Some(amount) = number_arg(ctx, 1)? else {
            return Ok(None);
        };
        dateadd(&part, amount, &ctx.get::<Value>(2)?)
    })?;
    connection.create_scalar_function("datediff", 3, deterministic, datediff_fn)?;
    connection.create_scalar_function("datediff_big", 3, deterministic, datediff_big_fn)?;
    for arity in [1, 2] {
        connection.create_scalar_function("eomonth", arity, deterministic, eomonth_fn)?;
    }
    connection.create_scalar_function("datetrunc", 2, deterministic, datetrunc_fn)?;
    for arity in [3, 4] {
        connection.create_scalar_function("date_bucket", arity, deterministic, date_bucket_fn)?;
    }
    for arity in [2, 3] {
        connection.create_scalar_function("tsql_convert", arity, deterministic, convert_fn)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection).unwrap();
        connection
    }

    fn text(connection: &Connection, sql: &str) -> String {
        connection
            .query_row(sql, [], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .unwrap_or_else(|| "NULL".to_owned())
    }

    fn int(connection: &Connection, sql: &str) -> i64 {
        connection.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn datepart_covers_every_part() {
        let c = conn();
        let d = "'2026-03-04 05:06:07.089'";
        for (part, expected) in [
            ("year", 2026),
            ("quarter", 1),
            ("month", 3),
            ("dayofyear", 63),
            ("day", 4),
            ("week", 10),
            ("weekday", 4),
            ("iso_week", 10),
            ("hour", 5),
            ("minute", 6),
            ("second", 7),
            ("millisecond", 89),
            ("microsecond", 89_000),
            ("nanosecond", 89_000_000),
            ("tzoffset", 0),
            ("yy", 2026),
            ("dw", 4),
            ("ww", 10),
        ] {
            assert_eq!(
                int(&c, &format!("SELECT datepart('{part}', {d})")),
                expected,
                "{part}"
            );
        }
        // ISO week edge cases: 2021-01-03 belongs to ISO week 53 of 2020,
        // 2024-12-30 to week 1 of 2025.
        assert_eq!(int(&c, "SELECT datepart('iso_week', '2021-01-03')"), 53);
        assert_eq!(int(&c, "SELECT datepart('iso_week', '2024-12-30')"), 1);
        assert!(
            c.query_row("SELECT datepart('fortnight', '2026-01-01')", [], |r| r
                .get::<_, i64>(0))
                .is_err()
        );
        assert!(
            c.query_row("SELECT datepart('year', 'not a date')", [], |r| r
                .get::<_, i64>(0))
                .is_err()
        );
    }

    #[test]
    fn datename_uses_english_names() {
        let c = conn();
        assert_eq!(text(&c, "SELECT datename('month', '2026-03-04')"), "March");
        assert_eq!(
            text(&c, "SELECT datename('weekday', '2026-03-04')"),
            "Wednesday"
        );
        assert_eq!(text(&c, "SELECT datename('year', '2026-03-04')"), "2026");
        assert_eq!(text(&c, "SELECT datename('dw', '2026-03-08')"), "Sunday");
        assert_eq!(
            text(&c, "SELECT datename('month', '2026-12-31')"),
            "December"
        );
    }

    #[test]
    fn datediff_counts_boundaries() {
        let c = conn();
        assert_eq!(
            int(
                &c,
                "SELECT datediff('day', '2026-03-04 23:00:00', '2026-03-05 01:00:00')"
            ),
            1
        );
        assert_eq!(
            int(&c, "SELECT datediff('year', '2020-12-31', '2021-01-01')"),
            1
        );
        assert_eq!(
            int(&c, "SELECT datediff('month', '2026-01-31', '2026-03-01')"),
            2
        );
        assert_eq!(
            int(&c, "SELECT datediff('quarter', '2026-03-31', '2026-04-01')"),
            1
        );
        assert_eq!(
            int(&c, "SELECT datediff('week', '2026-03-07', '2026-03-08')"),
            1
        );
        assert_eq!(
            int(&c, "SELECT datediff('week', '2026-03-02', '2026-03-07')"),
            0
        );
        assert_eq!(
            int(
                &c,
                "SELECT datediff('hour', '2026-03-04 01:59:59', '2026-03-04 02:00:00')"
            ),
            1
        );
        assert_eq!(
            int(&c, "SELECT datediff('day', '2026-03-10', '2026-03-04')"),
            -6
        );
        assert_eq!(
            int(
                &c,
                "SELECT datediff_big('millisecond', '2026-01-01', '2026-01-02')"
            ),
            86_400_000
        );
        assert!(
            c.query_row(
                "SELECT datediff('second', '1900-01-01', '2026-01-01')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .is_err(),
            "int overflow must be an error"
        );
    }

    #[test]
    fn dateadd_clamps_months_and_keeps_date_only_shape() {
        let c = conn();
        assert_eq!(
            text(&c, "SELECT dateadd('month', 1, '2026-01-31')"),
            "2026-02-28"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('month', 1, '2024-01-31')"),
            "2024-02-29"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('year', 1, '2024-02-29')"),
            "2025-02-28"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('quarter', 1, '2026-03-04')"),
            "2026-06-04"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('week', 2, '2026-03-04')"),
            "2026-03-18"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('day', -5, '2026-03-04')"),
            "2026-02-27"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('hour', 3, '2026-03-04')"),
            "2026-03-04 03:00:00"
        );
        assert_eq!(
            text(
                &c,
                "SELECT dateadd('microsecond', 100000, '2026-03-04 00:00:00')"
            ),
            "2026-03-04 00:00:00.1"
        );
        assert_eq!(
            text(&c, "SELECT dateadd('minute', 90, '2026-03-04 23:00:00')"),
            "2026-03-05 00:30:00"
        );
        assert_eq!(text(&c, "SELECT dateadd('day', 1, NULL)"), "NULL");
    }

    #[test]
    fn eomonth_trunc_and_bucket() {
        let c = conn();
        assert_eq!(text(&c, "SELECT eomonth('2024-02-10')"), "2024-02-29");
        assert_eq!(text(&c, "SELECT eomonth('2026-01-31', 1)"), "2026-02-28");
        assert_eq!(text(&c, "SELECT eomonth('2026-01-15', -1)"), "2025-12-31");
        assert_eq!(
            text(&c, "SELECT datetrunc('year', '2026-03-04 05:06:07')"),
            "2026-01-01 00:00:00"
        );
        assert_eq!(
            text(&c, "SELECT datetrunc('quarter', '2026-05-04')"),
            "2026-04-01"
        );
        assert_eq!(
            text(&c, "SELECT datetrunc('week', '2026-03-04')"),
            "2026-03-01"
        );
        assert_eq!(
            text(&c, "SELECT datetrunc('iso_week', '2026-03-04')"),
            "2026-03-02"
        );
        assert_eq!(
            text(&c, "SELECT datetrunc('hour', '2026-03-04 05:06:07')"),
            "2026-03-04 05:00:00"
        );
        assert_eq!(
            text(&c, "SELECT date_bucket('week', 1, '2026-03-04')"),
            "2026-03-02"
        );
        assert_eq!(
            text(&c, "SELECT date_bucket('day', 7, '2026-03-04')"),
            "2026-03-02"
        );
        assert_eq!(
            text(&c, "SELECT date_bucket('month', 3, '2026-05-04')"),
            "2026-04-01"
        );
        assert_eq!(
            text(
                &c,
                "SELECT date_bucket('hour', 4, '2026-03-04 05:06:07', '2026-03-04 00:00:00')"
            ),
            "2026-03-04 04:00:00"
        );
        assert!(
            c.query_row("SELECT date_bucket('day', 0, '2026-03-04')", [], |r| r
                .get::<_, String>(
                0
            ))
            .is_err()
        );
    }

    #[test]
    fn convert_styles() {
        let c = conn();
        let dt = "'2026-03-04 05:06:07.089'";
        for (style, expected) in [
            (100, "Mar  4 2026  5:06AM"),
            (101, "03/04/2026"),
            (103, "04/03/2026"),
            (104, "04.03.2026"),
            (105, "04-03-2026"),
            (106, "04 Mar 2026"),
            (107, "Mar 04, 2026"),
            (108, "05:06:07"),
            (110, "03-04-2026"),
            (111, "2026/03/04"),
            (112, "20260304"),
            (120, "2026-03-04 05:06:07"),
            (121, "2026-03-04 05:06:07.089"),
            (23, "2026-03-04"),
            (126, "2026-03-04T05:06:07.089"),
            (127, "2026-03-04T05:06:07.089Z"),
            (1, "03/04/26"),
            (12, "260304"),
        ] {
            assert_eq!(
                text(
                    &c,
                    &format!("SELECT tsql_convert('varchar(40)', {dt}, {style})")
                ),
                expected,
                "style {style}"
            );
        }
        assert_eq!(
            text(
                &c,
                "SELECT tsql_convert('varchar(10)', '2026-03-04 05:06:07', 120)"
            ),
            "2026-03-04"
        );
        assert!(
            c.query_row(
                "SELECT tsql_convert('varchar', '2026-03-04', 130)",
                [],
                |r| r.get::<_, String>(0)
            )
            .is_err()
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('date', '2026-03-04 05:06:07')"),
            "2026-03-04"
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('datetime', '2026-03-04')"),
            "2026-03-04 00:00:00.000"
        );
        assert_eq!(
            text(
                &c,
                "SELECT tsql_convert('datetime2', '2026-03-04 05:06:07.5')"
            ),
            "2026-03-04 05:06:07.5"
        );
        assert_eq!(
            text(
                &c,
                "SELECT tsql_convert('smalldatetime', '2026-03-04 05:06:37')"
            ),
            "2026-03-04 05:07:00"
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('time', '2026-03-04 05:06:07')"),
            "05:06:07"
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('date', '04/03/2026', 103)"),
            "2026-03-04"
        );
        assert_eq!(
            text(
                &c,
                "SELECT tsql_convert('datetime', '03/04/2026 10:11:12', 101)"
            ),
            "2026-03-04 10:11:12.000"
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('varchar', 1234.5, 1)"),
            "1,234.50"
        );
        assert_eq!(
            text(&c, "SELECT tsql_convert('varchar', 1234.5, 2)"),
            "1234.5000"
        );
        assert!(
            c.query_row("SELECT tsql_convert('date', 'garbage')", [], |r| r
                .get::<_, String>(0))
                .is_err()
        );
    }
}
