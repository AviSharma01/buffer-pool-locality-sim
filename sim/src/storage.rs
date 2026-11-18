//! What disk requests cost, by kind of storage.
//!
//! Storage only prices what the buffer pool already decided: a hit, a read
//! of a missed page, or a write-back of a modified page. It never changes
//! which pages are cached, so one run can be priced for every profile.
//!
//! The device serves requests. A read request costs a fixed part plus a
//! transfer per page. The fixed part depends on where the page is relative
//! to the last page read:
//!
//! - after a jump (backwards, or the first read): a full random request,
//!   which on a hard drive is a seek plus half a turn of the platter;
//! - further on: the head is already moving the right way, so a hard drive
//!   pays a small request overhead plus the time for the skipped pages to
//!   pass under it, never more than a random request. An SSD has no head,
//!   so its overhead equals its random request and gaps never matter.
//!
//! A write-back is a separate random write request; it does not change where
//! the read stream is. Figures are per 4 KB page, in milliseconds, from one
//! datasheet per drive (sources in the comments). Real databases usually
//! read through an operating-system file cache as well; that layer is not
//! modelled, so every miss goes to the device.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Ssd,
    Hdd,
}

/// Where a missed page was on the device, relative to the last page read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "read", rename_all = "snake_case")]
pub enum Read {
    /// The page right after the last one read.
    Next,
    /// Further on: `skipped` pages lie between it and the last one read.
    Ahead { skipped: u32 },
    /// Backwards, or the first read: a full random request.
    Jump,
}

impl Read {
    /// Classify a read of `page` when the last page read was `last`.
    pub fn of(page: u32, last: Option<u32>) -> Self {
        match last {
            Some(l) if page == l + 1 => Read::Next,
            Some(l) if page > l => Read::Ahead {
                skipped: page - l - 1,
            },
            _ => Read::Jump,
        }
    }
}

/// Request costs in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Profile {
    pub hit_ms: f64,
    /// Fixed part of a read request after a jump.
    pub read_request_ms: f64,
    /// Fixed part of a read request further on from the last page read.
    pub near_request_ms: f64,
    /// Fixed part of a write request.
    pub write_request_ms: f64,
    /// Transferring one page; also how long a skipped page takes to pass.
    pub page_ms: f64,
}

impl Profile {
    /// A read request of `pages` pages starting at a page classified as `read`.
    pub fn read_ms(&self, read: Read, pages: u32) -> f64 {
        let fixed = match read {
            Read::Next => self.near_request_ms.min(self.read_request_ms),
            Read::Ahead { skipped } => {
                (self.near_request_ms + f64::from(skipped) * self.page_ms).min(self.read_request_ms)
            }
            Read::Jump => self.read_request_ms,
        };
        fixed + f64::from(pages) * self.page_ms
    }

    /// Writing one modified page back.
    pub fn write_ms(&self) -> f64 {
        self.write_request_ms + self.page_ms
    }
}

impl StorageKind {
    pub const ALL: [StorageKind; 2] = [StorageKind::Ssd, StorageKind::Hdd];

    pub fn name(self) -> &'static str {
        match self {
            StorageKind::Ssd => "ssd",
            StorageKind::Hdd => "hdd",
        }
    }

    pub fn profile(self) -> Profile {
        match self {
            // Samsung 990 Pro datasheet, QD1: 22K random 4 KB reads/s
            // (about 45 µs each) and 80K writes/s (about 12.5 µs; the drive's
            // cache absorbs them); 7,450 MB/s sequential, so 0.6 µs a page.
            // No head: reading further on costs a full request.
            StorageKind::Ssd => Profile {
                hit_ms: 0.0001,
                read_request_ms: 0.044,
                near_request_ms: 0.044,
                write_request_ms: 0.012,
                page_ms: 0.0006,
            },
            // 7,200 rpm enterprise drive datasheet: average seek 8.5 ms read /
            // 9.0 ms write, average rotational latency 4.17 ms, 195 MiB/s
            // sustained (0.02 ms a page); QD1 random reads 78/s confirm about
            // 12.8 ms a request. The 0.2 ms overhead for reading further on is
            // an assumption (the drive's own read-ahead cache serves it; the
            // nearest datasheet figure is a 0.25 ms single-track seek).
            // Writes go through to the platter.
            StorageKind::Hdd => Profile {
                hit_ms: 0.0001,
                read_request_ms: 12.7,
                near_request_ms: 0.2,
                write_request_ms: 13.2,
                page_ms: 0.02,
            },
        }
    }
}

impl fmt::Display for StorageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for StorageKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StorageKind::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown storage {s:?} (expected ssd or hdd)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_classified_from_the_last_page_read() {
        assert_eq!(Read::of(7, None), Read::Jump);
        assert_eq!(Read::of(7, Some(6)), Read::Next);
        assert_eq!(Read::of(9, Some(6)), Read::Ahead { skipped: 2 });
        assert_eq!(Read::of(6, Some(6)), Read::Jump);
        assert_eq!(Read::of(2, Some(6)), Read::Jump);
    }

    #[test]
    fn a_hard_drive_charges_for_distance_and_an_ssd_does_not() {
        let hdd = StorageKind::Hdd.profile();
        let near = hdd.read_ms(Read::Next, 1);
        let three_on = hdd.read_ms(Read::Ahead { skipped: 3 }, 1);
        let jump = hdd.read_ms(Read::Jump, 1);
        assert!(near < three_on && three_on < jump);
        assert!((three_on - near - 3.0 * hdd.page_ms).abs() < 1e-12);
        // Far enough ahead, a seek is cheaper than waiting: capped.
        assert_eq!(hdd.read_ms(Read::Ahead { skipped: 100_000 }, 1), jump);
        let ssd = StorageKind::Ssd.profile();
        for read in [Read::Next, Read::Ahead { skipped: 3 }, Read::Jump] {
            assert_eq!(ssd.read_ms(read, 1), ssd.read_ms(Read::Jump, 1));
        }
        // A longer request adds only transfer.
        assert!(
            (ssd.read_ms(Read::Jump, 8) - ssd.read_ms(Read::Jump, 1) - 7.0 * ssd.page_ms).abs()
                < 1e-12
        );
    }

    #[test]
    fn the_figures_match_their_datasheets() {
        let ssd = StorageKind::Ssd.profile();
        let reads_per_s = 1000.0 / ssd.read_ms(Read::Jump, 1);
        let writes_per_s = 1000.0 / ssd.write_ms();
        assert!((21_000.0..23_000.0).contains(&reads_per_s), "{reads_per_s}");
        assert!(
            (78_000.0..82_000.0).contains(&writes_per_s),
            "{writes_per_s}"
        );
        let hdd = StorageKind::Hdd.profile();
        let reads_per_s = 1000.0 / hdd.read_ms(Read::Jump, 1);
        assert!((76.0..80.0).contains(&reads_per_s), "{reads_per_s}");
    }

    #[test]
    fn names_round_trip() {
        for kind in StorageKind::ALL {
            assert_eq!(kind.name().parse::<StorageKind>(), Ok(kind));
        }
        assert!(
            "tape"
                .parse::<StorageKind>()
                .unwrap_err()
                .contains("ssd or hdd")
        );
    }
}
