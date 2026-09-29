//! Application-level storage and write controls shared by every workflow.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Named starting points for resource behavior. Explicit command-line values
/// are applied after the profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceProfile {
    #[default]
    Balanced,
    LowWrite,
    Durable,
}

/// Fully resolved policy. This—not merely the preset name—is printed at startup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePolicy {
    pub profile: ResourceProfile,
    pub optional_exports: bool,
    pub reuse_content_addressed: bool,
    pub max_heavy_jobs: usize,
    pub ledger_batch_size: usize,
    pub max_write_bytes_per_second: Option<u64>,
    pub max_write_bytes_per_day: Option<u64>,
    pub min_free_space_bytes: u64,
    pub ledger_synchronous: String,
}

/// `StoragePolicy` is an application-facing synonym retained for callers that
/// describe these controls in storage terminology.
pub type StoragePolicy = ResourcePolicy;

impl ResourcePolicy {
    #[must_use]
    pub fn for_profile(profile: ResourceProfile) -> Self {
        match profile {
            ResourceProfile::Balanced => Self {
                profile,
                optional_exports: true,
                reuse_content_addressed: true,
                max_heavy_jobs: 4,
                ledger_batch_size: 16,
                max_write_bytes_per_second: None,
                max_write_bytes_per_day: None,
                min_free_space_bytes: 512 * 1024 * 1024,
                ledger_synchronous: "NORMAL".into(),
            },
            ResourceProfile::LowWrite => Self {
                profile,
                optional_exports: false,
                reuse_content_addressed: true,
                max_heavy_jobs: 1,
                ledger_batch_size: 32,
                max_write_bytes_per_second: Some(16 * 1024 * 1024),
                max_write_bytes_per_day: Some(4 * 1024 * 1024 * 1024),
                min_free_space_bytes: 1024 * 1024 * 1024,
                // Correctness and durability are never traded for fewer writes.
                ledger_synchronous: "NORMAL".into(),
            },
            ResourceProfile::Durable => Self {
                profile,
                optional_exports: true,
                reuse_content_addressed: true,
                max_heavy_jobs: 2,
                ledger_batch_size: 1,
                max_write_bytes_per_second: None,
                max_write_bytes_per_day: None,
                min_free_space_bytes: 2 * 1024 * 1024 * 1024,
                ledger_synchronous: "FULL".into(),
            },
        }
    }
}

/// Resource counters included in machine-readable and evaluation output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceMetrics {
    pub logical_bytes_generated: u64,
    pub physical_bytes_written: Option<u64>,
    pub bytes_avoided_deduplication: u64,
    pub wal_bytes: u64,
    pub checkpoint_bytes: u64,
    pub temporary_bytes: u64,
    pub cache_growth_bytes: i64,
    pub free_space_headroom_bytes: Option<u64>,
    pub resource_limit_failures: u64,
}

#[derive(Debug, Error)]
pub enum ResourceLimitError {
    #[error("daily write budget exceeded: {requested} requested, {remaining} remaining")]
    Daily { requested: u64, remaining: u64 },
    #[error("write-rate budget exceeded: {requested} bytes exceeds {limit} bytes/s")]
    Rate { requested: u64, limit: u64 },
}

/// Per-process measurement window. Linux exposes actual block-device write
/// bytes in `/proc/self/io`; other platforms report `None` rather than inventing it.
pub struct ResourceMonitor {
    started: Instant,
    physical_start: Option<u64>,
    cache_start: u64,
    metrics: ResourceMetrics,
}

impl ResourceMonitor {
    #[must_use]
    pub fn start(cache: Option<&Path>) -> Self {
        Self {
            started: Instant::now(),
            physical_start: physical_write_bytes(),
            cache_start: cache.map_or(0, directory_bytes),
            metrics: ResourceMetrics::default(),
        }
    }

    pub fn account_write(
        &mut self,
        bytes: u64,
        policy: &ResourcePolicy,
    ) -> Result<(), ResourceLimitError> {
        self.metrics.logical_bytes_generated =
            self.metrics.logical_bytes_generated.saturating_add(bytes);
        if let Some(limit) = policy.max_write_bytes_per_day {
            let remaining =
                limit.saturating_sub(self.metrics.logical_bytes_generated.saturating_sub(bytes));
            if bytes > remaining {
                self.metrics.resource_limit_failures += 1;
                return Err(ResourceLimitError::Daily {
                    requested: bytes,
                    remaining,
                });
            }
        }
        if let Some(limit) = policy.max_write_bytes_per_second {
            let allowed = limit.saturating_mul(self.started.elapsed().as_secs().max(1));
            if self.metrics.logical_bytes_generated > allowed {
                self.metrics.resource_limit_failures += 1;
                return Err(ResourceLimitError::Rate {
                    requested: bytes,
                    limit,
                });
            }
        }
        Ok(())
    }

    pub fn avoided(&mut self, bytes: u64) {
        self.metrics.bytes_avoided_deduplication += bytes;
    }

    #[must_use]
    pub fn finish(mut self, cache: Option<&Path>, headroom_path: &Path) -> ResourceMetrics {
        self.metrics.physical_bytes_written = match (self.physical_start, physical_write_bytes()) {
            (Some(a), Some(b)) => Some(b.saturating_sub(a)),
            _ => None,
        };
        let end = cache.map_or(0, directory_bytes);
        self.metrics.cache_growth_bytes = i64::try_from(end)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(self.cache_start).unwrap_or(i64::MAX));
        self.metrics.free_space_headroom_bytes = free_space(headroom_path);
        self.metrics
    }
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| e.path())
        .map(|p| {
            fs::metadata(&p).map_or(0, |m| {
                if m.is_dir() {
                    directory_bytes(&p)
                } else {
                    m.len()
                }
            })
        })
        .sum()
}

#[cfg(target_os = "linux")]
fn physical_write_bytes() -> Option<u64> {
    fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes: ")?.parse().ok())
}
#[cfg(not(target_os = "linux"))]
fn physical_write_bytes() -> Option<u64> {
    None
}

fn free_space(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["-Pk", path.to_str()?])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines()
        .last()?
        .split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

/// Write bytes only when content changed, accounting generated and avoided bytes.
pub fn write_if_changed(
    path: &Path,
    bytes: &[u8],
    policy: &ResourcePolicy,
    monitor: &mut ResourceMonitor,
) -> anyhow::Result<()> {
    monitor.account_write(bytes.len() as u64, policy)?;
    if policy.reuse_content_addressed && fs::read(path).is_ok_and(|old| old == bytes) {
        monitor.avoided(bytes.len() as u64);
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_write_is_bounded_without_weakening_sync() {
        let p = ResourcePolicy::for_profile(ResourceProfile::LowWrite);
        assert!(!p.optional_exports);
        assert_eq!(p.max_heavy_jobs, 1);
        assert_eq!(p.ledger_synchronous, "NORMAL");
        assert!(p.max_write_bytes_per_day.is_some());
    }
}
