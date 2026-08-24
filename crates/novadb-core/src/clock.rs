use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

const PHYSICAL_WIDTH: usize = 16;
const COUNTER_WIDTH: usize = 8;

/// A small hybrid logical clock suitable for deterministic last-write-wins
/// ordering.
///
/// Its textual representation is fixed-width hexadecimal, so lexical and
/// chronological ordering are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridLogicalClock {
    physical_ms: u64,
    counter: u32,
}

impl Default for HybridLogicalClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HybridLogicalClock {
    /// Creates a clock starting at the current Unix time.
    #[must_use]
    pub fn new() -> Self {
        Self {
            physical_ms: unix_time_ms(),
            counter: 0,
        }
    }

    /// Restores a clock from a previously emitted timestamp.
    pub fn from_timestamp(timestamp: &str) -> Result<Self> {
        let (physical, counter) = parse_timestamp(timestamp)?;
        Ok(Self {
            physical_ms: physical,
            counter,
        })
    }

    /// Advances the clock for a local event and returns its timestamp.
    pub fn tick(&mut self) -> String {
        let now = unix_time_ms();
        if now > self.physical_ms {
            self.physical_ms = now;
            self.counter = 0;
        } else {
            self.increment_counter();
        }
        self.timestamp()
    }

    /// Observes a remote timestamp, advancing this clock causally.
    pub fn observe(&mut self, remote: &str) -> Result<String> {
        let (remote_physical, remote_counter) = parse_timestamp(remote)?;
        let now = unix_time_ms();
        let local_physical = self.physical_ms;
        let local_counter = self.counter;
        let maximum = now.max(local_physical).max(remote_physical);

        self.physical_ms = maximum;
        self.counter = if maximum == local_physical && maximum == remote_physical {
            local_counter.max(remote_counter)
        } else if maximum == local_physical {
            local_counter
        } else if maximum == remote_physical {
            remote_counter
        } else {
            0
        };

        if maximum == now && maximum > local_physical && maximum > remote_physical {
            self.counter = 0;
        } else {
            self.increment_counter();
        }
        Ok(self.timestamp())
    }

    /// Returns the current timestamp without advancing the clock.
    #[must_use]
    pub fn timestamp(self) -> String {
        encode_timestamp(self.physical_ms, self.counter)
    }

    fn increment_counter(&mut self) {
        if let Some(next) = self.counter.checked_add(1) {
            self.counter = next;
        } else {
            self.physical_ms = self.physical_ms.saturating_add(1);
            self.counter = 0;
        }
    }
}

pub(crate) fn timestamp_physical_ms(value: &str) -> Result<u64> {
    parse_timestamp(value).map(|(physical_ms, _)| physical_ms)
}

fn parse_timestamp(value: &str) -> Result<(u64, u32)> {
    let Some((physical, counter)) = value.split_once('-') else {
        return Err(Error::InvalidHlc(value.to_owned()));
    };
    if physical.len() != PHYSICAL_WIDTH
        || counter.len() != COUNTER_WIDTH
        || !physical.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !counter.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::InvalidHlc(value.to_owned()));
    }

    let physical_ms =
        u64::from_str_radix(physical, 16).map_err(|_| Error::InvalidHlc(value.to_owned()))?;
    let logical_counter =
        u32::from_str_radix(counter, 16).map_err(|_| Error::InvalidHlc(value.to_owned()))?;
    let canonical = encode_timestamp(physical_ms, logical_counter);
    if canonical != value {
        return Err(Error::InvalidHlc(value.to_owned()));
    }
    Ok((physical_ms, logical_counter))
}

fn encode_timestamp(physical_ms: u64, counter: u32) -> String {
    format!("{physical_ms:016x}-{counter:08x}")
}

#[must_use]
pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_sort_lexically() {
        let mut clock = HybridLogicalClock::from_timestamp("0000000000000001-00000001").unwrap();
        let first = clock.timestamp();
        let second = clock.tick();
        assert!(second > first);
    }

    #[test]
    fn rejects_non_canonical_timestamp() {
        assert!(HybridLogicalClock::from_timestamp("1-1").is_err());
        assert!(HybridLogicalClock::from_timestamp("000000000000000A-00000001").is_err());
    }

    #[test]
    fn tick_is_monotonic_across_multiple_calls() {
        let mut clock = HybridLogicalClock::new();
        let mut previous = clock.tick();
        for _ in 0..100 {
            let current = clock.tick();
            assert!(current > previous, "{current} should be > {previous}");
            previous = current;
        }
    }

    #[test]
    fn observe_advances_past_remote_timestamp() {
        let mut local = HybridLogicalClock::from_timestamp("0000000000000010-00000000").unwrap();
        let remote = "0000000000000020-00000005";
        let result = local.observe(remote).unwrap();
        // The observed timestamp must be greater than the remote one
        assert!(result.as_str() > remote, "{result} should be > {remote}");
    }

    #[test]
    fn observe_advances_past_local_when_remote_is_older() {
        let mut clock = HybridLogicalClock::from_timestamp("0000000000000050-00000003").unwrap();
        let before = clock.timestamp();
        let after = clock.observe("0000000000000010-00000001").unwrap();
        assert!(after > before, "{after} should be > {before}");
    }

    #[test]
    fn from_timestamp_roundtrip() {
        let original = "00000191a1b2c3d4-0000002a";
        let clock = HybridLogicalClock::from_timestamp(original).unwrap();
        assert_eq!(clock.timestamp(), original);
    }

    #[test]
    fn encode_decode_boundary_values() {
        // Zero
        let ts = encode_timestamp(0, 0);
        assert_eq!(ts, "0000000000000000-00000000");
        let (p, c) = parse_timestamp(&ts).unwrap();
        assert_eq!(p, 0);
        assert_eq!(c, 0);

        // Max counter
        let ts = encode_timestamp(1, u32::MAX);
        let (p, c) = parse_timestamp(&ts).unwrap();
        assert_eq!(p, 1);
        assert_eq!(c, u32::MAX);
    }

    #[test]
    fn counter_overflow_rolls_physical_forward() {
        let mut clock = HybridLogicalClock {
            physical_ms: 100,
            counter: u32::MAX,
        };
        clock.increment_counter();
        assert_eq!(clock.physical_ms, 101);
        assert_eq!(clock.counter, 0);
    }

    #[test]
    fn rejects_invalid_timestamp_formats() {
        let invalids = [
            "",
            "abc",
            "0000000000000001",
            "-00000001",
            "0000000000000001-",
            "000000000000000g-00000001",   // non-hex
            "0000000000000001-0000000g",   // non-hex
            "00000000000000001-00000001",  // too long physical
            "0000000000000001-000000001",  // too long counter
        ];
        for input in invalids {
            assert!(
                parse_timestamp(input).is_err(),
                "should reject: {input:?}"
            );
        }
    }

    #[test]
    fn fixed_width_is_exactly_25_characters() {
        let ts = encode_timestamp(42, 7);
        assert_eq!(ts.len(), 25); // 16 + '-' + 8
    }
}

