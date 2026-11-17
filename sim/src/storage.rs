//! What disk reads and write-backs cost, by kind of storage.
//!
//! Storage only prices what the buffer pool already decided: a hit, a
//! sequential read (the page right after the last one the disk touched), a
//! random read, or a write-back of a modified page. It never changes which
//! pages are cached, so one run can be priced for every profile.
//!
//! Latencies are rough per-page (4 KB) figures for typical hardware, in
//! milliseconds. They are there for their ratios, not as benchmarks. Real
//! databases usually read through an operating-system file cache as well;
//! that layer is not modelled, so every miss goes to the device.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Ssd,
    Hdd,
}

/// How a missed page came off the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Read {
    /// The page after the last one the device touched.
    Sequential,
    Random,
}

/// Milliseconds per page for each kind of access.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Profile {
    pub hit_ms: f64,
    pub sequential_read_ms: f64,
    pub random_read_ms: f64,
    /// Writing a modified page back; treated as a random write.
    pub write_ms: f64,
}

impl Profile {
    pub fn read_ms(&self, read: Read) -> f64 {
        match read {
            Read::Sequential => self.sequential_read_ms,
            Read::Random => self.random_read_ms,
        }
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
            // NVMe-class: no moving parts, but a random 4 KB read is still a
            // separate request, several times slower than streaming.
            StorageKind::Ssd => Profile {
                hit_ms: 0.0001,
                sequential_read_ms: 0.02,
                random_read_ms: 0.1,
                write_ms: 0.1,
            },
            // 7200 rpm: about 4 ms to move the head and 4 ms for the platter
            // to come round; streaming the next page costs only its transfer.
            StorageKind::Hdd => Profile {
                hit_ms: 0.0001,
                sequential_read_ms: 0.03,
                random_read_ms: 8.0,
                write_ms: 8.0,
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
    fn a_random_read_costs_more_than_a_sequential_one_everywhere() {
        for kind in StorageKind::ALL {
            let p = kind.profile();
            assert!(p.hit_ms < p.sequential_read_ms, "{kind}");
            assert!(p.sequential_read_ms < p.random_read_ms, "{kind}");
        }
        // The point of the hard-drive profile: random is far worse.
        let hdd = StorageKind::Hdd.profile();
        assert!(hdd.random_read_ms / hdd.sequential_read_ms > 100.0);
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
