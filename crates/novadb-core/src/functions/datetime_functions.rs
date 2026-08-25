//! Extended date/time functions.
//!
//! Provides `NOW_MS()`, `NOW_ISO()`, `DATE_TRUNC()`, `DATE_PART()`, `EPOCH_MS()`,
//! `FORMAT_TIMESTAMP()`, and more.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

use crate::Result;

/// Registers date/time functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    // NOW_MS() — Current Unix timestamp in milliseconds
    connection.create_scalar_function("now_ms", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0))
    })?;

    // NOW_US() — Current Unix timestamp in microseconds
    connection.create_scalar_function("now_us", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0))
    })?;

    // NOW_ISO() — Current time as ISO 8601 string (UTC)
    connection.create_scalar_function("now_iso", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();

        // Calculate date components from epoch
        let days = secs / 86_400;
        let time_secs = secs % 86_400;
        let hours = time_secs / 3_600;
        let minutes = (time_secs % 3_600) / 60;
        let seconds = time_secs % 60;

        // Days to Y-M-D (civil calendar from Unix epoch)
        let (year, month, day) = days_to_ymd(days as i64);

        Ok(format!(
            "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"
        ))
    })?;

    // GETDATE(), SYSDATETIME(), NOW() — SQL Server / MySQL aliases
    connection.create_scalar_function("getdate", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        let days = secs / 86_400;
        let time_secs = secs % 86_400;
        let hours = time_secs / 3_600;
        let minutes = (time_secs % 3_600) / 60;
        let seconds = time_secs % 60;
        let (year, month, day) = days_to_ymd(days as i64);
        Ok(format!(
            "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"
        ))
    })?;
    connection.create_scalar_function("sysdatetime", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        let days = secs / 86_400;
        let time_secs = secs % 86_400;
        let hours = time_secs / 3_600;
        let minutes = (time_secs % 3_600) / 60;
        let seconds = time_secs % 60;
        let (year, month, day) = days_to_ymd(days as i64);
        Ok(format!(
            "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"
        ))
    })?;
    connection.create_scalar_function("now", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        let days = secs / 86_400;
        let time_secs = secs % 86_400;
        let hours = time_secs / 3_600;
        let minutes = (time_secs % 3_600) / 60;
        let seconds = time_secs % 60;
        let (year, month, day) = days_to_ymd(days as i64);
        Ok(format!(
            "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z"
        ))
    })?;

    // EPOCH_MS(iso_text) — Convert ISO 8601 string to Unix ms
    connection.create_scalar_function(
        "epoch_ms",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match parse_iso8601_to_epoch_ms(&text) {
                Some(ms) => Ok(Some(ms)),
                None => Ok(None),
            }
        },
    )?;

    // FROM_EPOCH_MS(ms) — Convert Unix ms to ISO 8601 string
    connection.create_scalar_function(
        "from_epoch_ms",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let ms: i64 = ctx.get(0)?;
            Ok(Some(epoch_ms_to_iso(ms)))
        },
    )?;

    // DATE_PART(part, iso_text) — Extract part from ISO 8601 string
    connection.create_scalar_function(
        "date_part",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let part: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            let ms = match parse_iso8601_to_epoch_ms(&text) {
                Some(ms) => ms,
                None => return Ok(None),
            };

            let secs = ms / 1_000;
            let days = secs / 86_400;
            let time_secs = secs % 86_400;
            let (year, month, day) = days_to_ymd(days);

            let result = match part.to_lowercase().as_str() {
                "year" => year as i64,
                "month" => month as i64,
                "day" => day as i64,
                "hour" => time_secs / 3_600,
                "minute" => (time_secs % 3_600) / 60,
                "second" => time_secs % 60,
                "millisecond" => ms % 1_000,
                "dow" | "dayofweek" => (days + 4) % 7, // Unix epoch was Thursday (4)
                "doy" | "dayofyear" => day_of_year(year as i32, month as u32, day as u32) as i64,
                "epoch" => secs,
                _ => return Ok(None),
            };
            Ok(Some(result))
        },
    )?;

    // DATEPART(part, iso_text) — T-SQL alias for date_part
    connection.create_scalar_function(
        "datepart",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let part: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            let ms = match parse_iso8601_to_epoch_ms(&text) {
                Some(ms) => ms,
                None => return Ok(None),
            };

            let secs = ms / 1_000;
            let days = secs / 86_400;
            let time_secs = secs % 86_400;
            let (year, month, day) = days_to_ymd(days);

            let result = match part.to_lowercase().as_str() {
                "year" | "yy" | "yyyy" => year as i64,
                "quarter" | "qq" | "q" => ((month as i64 - 1) / 3) + 1,
                "month" | "mm" | "m" => month as i64,
                "day" | "dd" | "d" => day as i64,
                "hour" | "hh" => time_secs / 3_600,
                "minute" | "mi" | "n" => (time_secs % 3_600) / 60,
                "second" | "ss" | "s" => time_secs % 60,
                "millisecond" | "ms" => ms % 1_000,
                "dayofweek" | "dw" => (days + 4) % 7,
                "dayofyear" | "dy" => day_of_year(year as i32, month as u32, day as u32) as i64,
                "week" | "wk" | "ww" => {
                    let doy = day_of_year(year as i32, month as u32, day as u32) as i64;
                    (doy - 1) / 7 + 1
                }
                _ => return Ok(None),
            };
            Ok(Some(result))
        },
    )?;

    // DATE_TRUNC(part, iso_text) — Truncate to specified precision
    connection.create_scalar_function(
        "date_trunc",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let part: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            let ms = match parse_iso8601_to_epoch_ms(&text) {
                Some(ms) => ms,
                None => return Ok(None),
            };

            let secs = ms / 1_000;
            let days = secs / 86_400;
            let time_secs = secs % 86_400;
            let (year, month, _day) = days_to_ymd(days);

            let truncated_ms = match part.to_lowercase().as_str() {
                "year" => ymd_to_days(year as i32, 1, 1) * 86_400_000,
                "month" => ymd_to_days(year as i32, month as u32, 1) * 86_400_000,
                "day" => days * 86_400_000,
                "hour" => {
                    let hours = time_secs / 3_600;
                    days * 86_400_000 + hours * 3_600_000
                }
                "minute" => {
                    let minutes = time_secs / 60;
                    days * 86_400_000 + minutes * 60_000
                }
                "second" => secs * 1_000,
                _ => return Ok(None),
            };
            Ok(Some(epoch_ms_to_iso(truncated_ms)))
        },
    )?;

    // AGE_MS(iso1, iso2) — Difference in milliseconds between two timestamps
    connection.create_scalar_function(
        "age_ms",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text1: String = ctx.get(0)?;
            let text2: String = ctx.get(1)?;
            match (
                parse_iso8601_to_epoch_ms(&text1),
                parse_iso8601_to_epoch_ms(&text2),
            ) {
                (Some(ms1), Some(ms2)) => Ok(Some(ms1 - ms2)),
                _ => Ok(None),
            }
        },
    )?;

    // YEAR(date_val) — T-SQL Year extract
    connection.create_scalar_function(
        "year",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            if let Some(ms) = parse_iso8601_to_epoch_ms(&text) {
                let days = (ms / 1_000) / 86_400;
                let (year, _, _) = days_to_ymd(days);
                Ok(Some(year as i64))
            } else {
                Ok(None)
            }
        },
    )?;

    // MONTH(date_val) — T-SQL Month extract
    connection.create_scalar_function(
        "month",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            if let Some(ms) = parse_iso8601_to_epoch_ms(&text) {
                let days = (ms / 1_000) / 86_400;
                let (_, month, _) = days_to_ymd(days);
                Ok(Some(month as i64))
            } else {
                Ok(None)
            }
        },
    )?;

    // DAY(date_val) — T-SQL Day extract
    connection.create_scalar_function(
        "day",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            if let Some(ms) = parse_iso8601_to_epoch_ms(&text) {
                let days = (ms / 1_000) / 86_400;
                let (_, _, day) = days_to_ymd(days);
                Ok(Some(day as i64))
            } else {
                Ok(None)
            }
        },
    )?;

    // DATEADD(part, num, date_str) — T-SQL Date Addition
    connection.create_scalar_function(
        "dateadd",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let part: String = ctx.get(0)?;
            let num: i64 = ctx.get(1)?;
            let text: String = ctx.get(2)?;
            let ms = match parse_iso8601_to_epoch_ms(&text) {
                Some(ms) => ms,
                None => return Ok(None),
            };
            let secs = ms / 1_000;
            let days = secs / 86_400;
            let time_secs = secs % 86_400;
            let (y, m, d) = days_to_ymd(days);

            let new_ms = match part.to_lowercase().as_str() {
                "day" | "d" | "dd" => ms + num * 86_400_000,
                "hour" | "hh" | "h" => ms + num * 3_600_000,
                "minute" | "mi" | "n" => ms + num * 60_000,
                "second" | "ss" | "s" => ms + num * 1_000,
                "millisecond" | "ms" => ms + num,
                "month" | "mm" | "m" => {
                    let total_months = (y as i64 * 12 + (m as i64 - 1)) + num;
                    let new_y = total_months.div_euclid(12) as i32;
                    let new_m = (total_months.rem_euclid(12) + 1) as u32;
                    let new_days = ymd_to_days(new_y, new_m, d as u32);
                    new_days * 86_400_000 + time_secs * 1_000 + (ms % 1_000)
                }
                "year" | "yy" | "yyyy" => {
                    let new_y = y as i32 + num as i32;
                    let new_days = ymd_to_days(new_y, m as u32, d as u32);
                    new_days * 86_400_000 + time_secs * 1_000 + (ms % 1_000)
                }
                _ => return Ok(None),
            };
            Ok(Some(epoch_ms_to_iso(new_ms)))
        },
    )?;

    // DATEDIFF(part, start_str, end_str) — T-SQL Date Difference
    connection.create_scalar_function(
        "datediff",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let part: String = ctx.get(0)?;
            let text1: String = ctx.get(1)?;
            let text2: String = ctx.get(2)?;
            let ms1 = match parse_iso8601_to_epoch_ms(&text1) {
                Some(ms) => ms,
                None => return Ok(None),
            };
            let ms2 = match parse_iso8601_to_epoch_ms(&text2) {
                Some(ms) => ms,
                None => return Ok(None),
            };
            let diff_ms = ms2 - ms1;
            let result = match part.to_lowercase().as_str() {
                "day" | "d" | "dd" => diff_ms / 86_400_000,
                "hour" | "hh" | "h" => diff_ms / 3_600_000,
                "minute" | "mi" | "n" => diff_ms / 60_000,
                "second" | "ss" | "s" => diff_ms / 1_000,
                "millisecond" | "ms" => diff_ms,
                "month" | "mm" | "m" => {
                    let (y1, m1, _) = days_to_ymd((ms1 / 1_000) / 86_400);
                    let (y2, m2, _) = days_to_ymd((ms2 / 1_000) / 86_400);
                    (y2 as i64 * 12 + m2 as i64) - (y1 as i64 * 12 + m1 as i64)
                }
                "year" | "yy" | "yyyy" => {
                    let (y1, _, _) = days_to_ymd((ms1 / 1_000) / 86_400);
                    let (y2, _, _) = days_to_ymd((ms2 / 1_000) / 86_400);
                    y2 as i64 - y1 as i64
                }
                _ => return Ok(None),
            };
            Ok(Some(result))
        },
    )?;

    // SYSDATETIMEOFFSET() -> string
    connection.create_scalar_function(
        "sysdatetimeoffset",
        0,
        FunctionFlags::SQLITE_UTF8,
        |_ctx| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            Ok(format!("{}+07:00", epoch_ms_to_iso(now)))
        },
    )?;

    // DATEFROMPARTS(year, month, day) -> 'YYYY-MM-DD'
    connection.create_scalar_function(
        "datefromparts",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let y: i64 = ctx.get(0)?;
            let m: i64 = ctx.get(1)?;
            let d: i64 = ctx.get(2)?;
            Ok(format!("{y:04}-{m:02}-{d:02}"))
        },
    )?;

    // EOMONTH(date_str) -> 'YYYY-MM-DD' (last day of month)
    connection.create_scalar_function(
        "eomonth",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            if let Some(ms) = parse_iso8601_to_epoch_ms(&text) {
                let days = (ms / 1_000) / 86_400;
                let (y, m, _) = days_to_ymd(days);
                let next_m_y = if m == 12 { y as i32 + 1 } else { y as i32 };
                let next_m = if m == 12 { 1u32 } else { (m + 1) as u32 };
                let first_day_next_month = ymd_to_days(next_m_y, next_m, 1);
                let last_day = days_to_ymd(first_day_next_month - 1);
                Ok(Some(format!(
                    "{:04}-{:02}-{:02}",
                    last_day.0, last_day.1, last_day.2
                )))
            } else {
                Ok(None)
            }
        },
    )?;

    // DB_NAME() -> 'NovaSqlServerLab'
    connection.create_scalar_function("db_name", 0, FunctionFlags::SQLITE_UTF8, |_ctx| {
        Ok("NovaSqlServerLab".to_string())
    })?;

    // SERVERPROPERTY(prop) -> property string
    connection.create_scalar_function("serverproperty", 1, FunctionFlags::SQLITE_UTF8, |ctx| {
        let prop: String = ctx.get(0)?;
        match prop.to_lowercase().as_str() {
            "productversion" => Ok("16.0.4095.4".to_string()),
            "productlevel" => Ok("RTM".to_string()),
            "edition" => Ok("Enterprise Edition: Core-based (64-bit)".to_string()),
            _ => Ok("NovaDB Enterprise".to_string()),
        }
    })?;

    // DATETRUNC(part, datetime) -> truncated datetime string
    connection.create_scalar_function("datetrunc", 2, FunctionFlags::SQLITE_UTF8, |ctx| {
        let part: String = ctx.get(0)?;
        let text: String = ctx.get(1)?;
        if let Some(ms) = parse_iso8601_to_epoch_ms(&text) {
            let secs = ms / 1_000;
            let days = secs / 86_400;
            let (y, m, d) = days_to_ymd(days);
            let day_secs = (secs % 86_400) as u32;
            let h = day_secs / 3600;
            let mi = (day_secs % 3600) / 60;
            let result = match part.to_lowercase().as_str() {
                "year" | "yy" | "yyyy" => format!("{y:04}-01-01 00:00:00.000"),
                "quarter" | "qq" | "q" => {
                    let qm = ((m - 1) / 3) * 3 + 1;
                    format!("{y:04}-{qm:02}-01 00:00:00.000")
                }
                "month" | "mm" | "m" => format!("{y:04}-{m:02}-01 00:00:00.000"),
                "day" | "dd" | "d" => format!("{y:04}-{m:02}-{d:02} 00:00:00.000"),
                "hour" | "hh" => format!("{y:04}-{m:02}-{d:02} {h:02}:00:00.000"),
                "minute" | "mi" | "n" => format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:00.000"),
                _ => format!("{y:04}-{m:02}-{d:02} 00:00:00.000"),
            };
            Ok(Some(result))
        } else {
            Ok(None)
        }
    })?;

    Ok(())
}

// --- Date utility functions ---

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn days_to_ymd(days_since_epoch: i64) -> (i64, i64, i64) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as i64, d as i64)
}

fn ymd_to_days(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 {
        year as i64 - 1
    } else {
        year as i64
    };
    let m = if month <= 2 {
        month as i64 + 9
    } else {
        month as i64 - 3
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m as u32 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn day_of_year(year: i32, month: u32, day: u32) -> u32 {
    let mut doy = day;
    for m in 1..month {
        doy += days_in_month(year, m);
    }
    doy
}

fn parse_iso8601_to_epoch_ms(text: &str) -> Option<i64> {
    // Parse formats: YYYY-MM-DDTHH:MM:SS.mmmZ or YYYY-MM-DD HH:MM:SS or YYYY-MM-DD
    let text = text.trim();
    if text.len() < 10 {
        return None;
    }

    let year: i32 = text[..4].parse().ok()?;
    if text.as_bytes().get(4) != Some(&b'-') {
        return None;
    }
    let month: u32 = text[5..7].parse().ok()?;
    if text.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let day: u32 = text[8..10].parse().ok()?;

    if month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }

    let days = ymd_to_days(year, month, day);
    let mut ms = days * 86_400_000;

    if text.len() > 10 {
        let sep = text.as_bytes()[10];
        if sep != b'T' && sep != b' ' {
            return None;
        }
        let time_part = text[11..].trim_end_matches('Z');

        let parts: Vec<&str> = time_part.splitn(2, '.').collect();
        let hms: Vec<&str> = parts[0].split(':').collect();

        if hms.len() >= 2 {
            let hours: i64 = hms[0].parse().ok()?;
            let minutes: i64 = hms[1].parse().ok()?;
            let seconds: i64 = if hms.len() >= 3 {
                hms[2].parse().ok()?
            } else {
                0
            };
            ms += hours * 3_600_000 + minutes * 60_000 + seconds * 1_000;
        }

        if parts.len() == 2 {
            let frac = parts[1];
            let frac_ms: i64 = match frac.len() {
                1 => frac.parse::<i64>().ok()? * 100,
                2 => frac.parse::<i64>().ok()? * 10,
                3 => frac.parse().ok()?,
                n if n > 3 => frac[..3].parse().ok()?,
                _ => 0,
            };
            ms += frac_ms;
        }
    }

    Some(ms)
}

fn epoch_ms_to_iso(ms: i64) -> String {
    let total_secs = ms / 1_000;
    let frac_ms = (ms % 1_000).unsigned_abs();
    let days = total_secs / 86_400;
    let time_secs = total_secs % 86_400;
    let hours = time_secs / 3_600;
    let minutes = (time_secs % 3_600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{frac_ms:03}Z")
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::register(&conn).unwrap();
        conn
    }

    #[test]
    fn now_ms_returns_plausible_value() {
        let conn = setup();
        let ms: i64 = conn
            .query_row("SELECT now_ms()", [], |row| row.get(0))
            .unwrap();
        // Should be after 2024-01-01 in milliseconds
        assert!(ms > 1_704_067_200_000);
    }

    #[test]
    fn now_iso_returns_valid_format() {
        let conn = setup();
        let iso: String = conn
            .query_row("SELECT now_iso()", [], |row| row.get(0))
            .unwrap();
        assert!(iso.ends_with('Z'));
        assert!(iso.contains('T'));
        assert_eq!(iso.len(), 24); // YYYY-MM-DDTHH:MM:SS.mmmZ
    }

    #[test]
    fn epoch_ms_and_from_epoch_ms_roundtrip() {
        let conn = setup();
        let original = "2024-06-15T14:30:45.123Z";
        let roundtripped: String = conn
            .query_row("SELECT from_epoch_ms(epoch_ms(?1))", [original], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(roundtripped, original);
    }

    #[test]
    fn epoch_ms_parses_date_only() {
        let conn = setup();
        let ms: Option<i64> = conn
            .query_row("SELECT epoch_ms('2024-01-01')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ms, Some(1_704_067_200_000));
    }

    #[test]
    fn date_part_extracts_components() {
        let conn = setup();
        let ts = "2024-06-15T14:30:45.123Z";

        let year: i64 = conn
            .query_row("SELECT date_part('year', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(year, 2024);

        let month: i64 = conn
            .query_row("SELECT date_part('month', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(month, 6);

        let day: i64 = conn
            .query_row("SELECT date_part('day', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(day, 15);

        let hour: i64 = conn
            .query_row("SELECT date_part('hour', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(hour, 14);
    }

    #[test]
    fn date_trunc_truncates_correctly() {
        let conn = setup();
        let ts = "2024-06-15T14:30:45.123Z";

        let truncated: String = conn
            .query_row("SELECT date_trunc('day', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(truncated, "2024-06-15T00:00:00.000Z");

        let truncated: String = conn
            .query_row("SELECT date_trunc('month', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(truncated, "2024-06-01T00:00:00.000Z");

        let truncated: String = conn
            .query_row("SELECT date_trunc('hour', ?1)", [ts], |row| row.get(0))
            .unwrap();
        assert_eq!(truncated, "2024-06-15T14:00:00.000Z");
    }

    #[test]
    fn age_ms_computes_difference() {
        let conn = setup();
        let diff: i64 = conn
            .query_row(
                "SELECT age_ms('2024-01-02T00:00:00.000Z', '2024-01-01T00:00:00.000Z')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(diff, 86_400_000); // 1 day in ms
    }
}
