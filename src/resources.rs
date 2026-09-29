//! Process resource detection and blocking admission control for batch extraction.

use std::sync::{Condvar, Mutex};

/// Resource limits used by an extraction batch.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Limits {
    pub max_memory_bytes: u64,
    pub max_inflight_bytes: u64,
    pub max_pages: u32,
    pub max_backend_sessions: usize,
}

impl Limits {
    /// Resolve omitted values conservatively. The explicit memory ceiling is
    /// always authoritative, even when the platform reports more memory.
    pub fn resolve(
        memory: Option<u64>,
        inflight: Option<u64>,
        pages: Option<u32>,
        sessions: Option<usize>,
    ) -> Self {
        let detected = available_memory_bytes();
        let max_memory_bytes = memory.unwrap_or_else(|| {
            detected
                .map_or(512 << 20, |n| n.saturating_mul(3) / 4)
                .max(64 << 20)
        });
        let max_inflight_bytes = inflight
            .unwrap_or_else(|| (max_memory_bytes / 2).max(1))
            .min(max_memory_bytes);
        Self {
            max_memory_bytes,
            max_inflight_bytes,
            max_pages: pages.unwrap_or(1_000),
            max_backend_sessions: sessions.unwrap_or(1).max(1),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResourceError {
    #[error("resource limit: input requires {requested} bytes, budget is {limit} bytes")]
    TooLarge { requested: u64, limit: u64 },
    #[error("resource admission cancelled")]
    Cancelled,
}

#[derive(Default)]
struct State {
    bytes: u64,
    sessions: usize,
    cancelled: bool,
}

/// Shared byte/session budget. `reserve` blocks (backpressure) rather than
/// allowing the caller to read a document while capacity is unavailable.
pub struct Budget {
    limits: Limits,
    state: Mutex<State>,
    changed: Condvar,
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub fn reserve(&self, bytes: u64) -> Result<Reservation<'_>, ResourceError> {
        if bytes > self.limits.max_inflight_bytes {
            return Err(ResourceError::TooLarge {
                requested: bytes,
                limit: self.limits.max_inflight_bytes,
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.cancelled {
                return Err(ResourceError::Cancelled);
            }
            let fits_bytes = state
                .bytes
                .checked_add(bytes)
                .is_some_and(|n| n <= self.limits.max_inflight_bytes);
            if fits_bytes && state.sessions < self.limits.max_backend_sessions {
                state.bytes += bytes;
                state.sessions += 1;
                return Ok(Reservation {
                    budget: self,
                    bytes,
                });
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
    pub fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        self.changed.notify_all();
    }
}

pub struct Reservation<'a> {
    budget: &'a Budget,
    bytes: u64,
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.bytes -= self.bytes;
        state.sessions -= 1;
        self.budget.changed.notify_all();
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Usage {
    pub peak_rss_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
fn number_file(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
pub fn available_memory_bytes() -> Option<u64> {
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let current = number_file("/sys/fs/cgroup/memory.current");
    if let (Some(limit), Some(current)) = (limit, current) {
        return Some(limit.saturating_sub(current));
    }
    meminfo_value("MemAvailable")
}

#[cfg(target_os = "linux")]
fn meminfo_value(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = text
        .lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    kb.checked_mul(1024)
}

#[cfg(target_os = "linux")]
fn process_status_value(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = text
        .lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    kb.checked_mul(1024)
}

#[cfg(target_os = "linux")]
pub fn usage() -> Usage {
    Usage {
        peak_rss_bytes: process_status_value("VmHWM"),
        swap_bytes: number_file("/sys/fs/cgroup/memory.swap.current")
            .or_else(|| process_status_value("VmSwap")),
    }
}

#[cfg(target_os = "macos")]
pub fn available_memory_bytes() -> Option<u64> {
    // `host_statistics64(HOST_VM_INFO64)` is the supported Mach VM pressure API.
    use std::ffi::c_void;
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_page_size(host: u32, size: *mut u32) -> i32;
        fn host_statistics64(host: u32, flavor: i32, info: *mut c_void, count: *mut u32) -> i32;
    }
    let mut page = 0u32;
    let mut values = [0u32; 64];
    let mut count = 64u32;
    if unsafe { host_page_size(mach_host_self(), &mut page) } != 0
        || unsafe { host_statistics64(mach_host_self(), 4, values.as_mut_ptr().cast(), &mut count) }
            != 0
    {
        return None;
    }
    u64::from(values[0])
        .checked_add(u64::from(values[2]))?
        .checked_add(u64::from(values[14]))?
        .checked_mul(u64::from(page))
}

#[cfg(target_os = "macos")]
pub fn usage() -> Usage {
    Usage {
        peak_rss_bytes: None,
        swap_bytes: None,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn available_memory_bytes() -> Option<u64> {
    None
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn usage() -> Usage {
    Usage {
        peak_rss_bytes: None,
        swap_bytes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    fn limits(bytes: u64) -> Limits {
        Limits {
            max_memory_bytes: bytes,
            max_inflight_bytes: bytes,
            max_pages: 1,
            max_backend_sessions: 2,
        }
    }
    #[test]
    fn releases_reservations() {
        let b = Budget::new(limits(10));
        {
            let _r = b.reserve(10).unwrap();
        }
        assert!(b.reserve(10).is_ok());
    }
    #[test]
    fn rejects_larger_than_total() {
        assert!(matches!(
            Budget::new(limits(2)).reserve(3),
            Err(ResourceError::TooLarge {
                requested: 3,
                limit: 2
            })
        ));
    }
    #[test]
    fn overflow_is_not_admitted() {
        let b = Budget::new(limits(u64::MAX));
        let _r = b.reserve(u64::MAX).unwrap();
        let state = b.state.lock().unwrap();
        assert!(state.bytes.checked_add(1).is_none());
    }
    #[test]
    fn waiting_admission_resumes() {
        let b = Arc::new(Budget::new(limits(4)));
        let r = b.reserve(4).unwrap();
        let other = Arc::clone(&b);
        let h = std::thread::spawn(move || other.reserve(1).is_ok());
        std::thread::sleep(Duration::from_millis(20));
        assert!(!h.is_finished());
        drop(r);
        assert!(h.join().unwrap());
    }
    #[test]
    fn cancellation_wakes_waiter() {
        let b = Arc::new(Budget::new(limits(4)));
        let _r = b.reserve(4).unwrap();
        let other = Arc::clone(&b);
        let h =
            std::thread::spawn(move || matches!(other.reserve(1), Err(ResourceError::Cancelled)));
        std::thread::sleep(Duration::from_millis(20));
        b.cancel();
        assert!(h.join().unwrap());
    }
}
