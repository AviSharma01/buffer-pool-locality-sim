//! How far the pool may read ahead. When to read ahead, and where pages read
//! ahead go, is each policy's choice (see `Policy::reads_ahead`).

use serde::Serialize;

/// How far to read ahead when a policy decides to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadAhead {
    #[default]
    Off,
    /// A quarter of the pool, at most 8 pages: real systems read ahead far
    /// less than 1% of their pool, and a bigger share would flush this one.
    Auto,
    /// Up to this many pages (still at most a quarter of the pool).
    Pages(u32),
}

impl ReadAhead {
    /// Most pages one read-ahead may load into a pool of `frames`.
    pub fn cap(self, frames: usize) -> usize {
        let quarter = frames / 4;
        match self {
            ReadAhead::Off => 0,
            ReadAhead::Auto => quarter.min(8),
            ReadAhead::Pages(n) => quarter.min(n as usize),
        }
    }
}

impl std::str::FromStr for ReadAhead {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "off" | "0" => Ok(ReadAhead::Off),
            "auto" => Ok(ReadAhead::Auto),
            n => n.parse().map(ReadAhead::Pages).map_err(|_| {
                format!("unknown read-ahead {s:?} (expected auto, off or a page count)")
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_ahead_is_capped_at_a_quarter_of_the_pool_and_eight_pages() {
        assert_eq!(ReadAhead::Auto.cap(3), 0);
        assert_eq!(ReadAhead::Auto.cap(8), 2);
        assert_eq!(ReadAhead::Auto.cap(100), 8);
        assert_eq!(ReadAhead::Pages(3).cap(8), 2);
        assert_eq!(ReadAhead::Pages(3).cap(100), 3);
        assert_eq!("off".parse(), Ok(ReadAhead::Off));
        assert_eq!("5".parse(), Ok(ReadAhead::Pages(5)));
        assert!("lots".parse::<ReadAhead>().is_err());
    }
}
