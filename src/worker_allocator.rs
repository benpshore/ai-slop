//! Stop disposable workers on allocation failure, including fallible allocation.
//!
//! A decoder can suppress an error from `Vec::try_reserve`/`Read::read_to_end`
//! and return a successful prefix. Once the native worker's OS limits and core
//! policy are installed, a null Rust allocation must end that worker instead.
//! Controllers and library users keep their normal allocator behavior. This is
//! not an additional memory limit and does not intercept native malloc/mmap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

struct WorkerAllocator;

#[global_allocator]
static ALLOCATOR: WorkerAllocator = WorkerAllocator;

fn checked(pointer: *mut u8) -> *mut u8 {
    if pointer.is_null() && ENABLED.load(Ordering::SeqCst) {
        allocation_failed();
    }
    pointer
}

#[cold]
fn allocation_failed() -> ! {
    // No formatting, locks, allocation or unwinding inside GlobalAlloc. In
    // particular, handle_alloc_error is permitted to unwind under some policies.
    // Worker stderr is a controller-owned capture file; failure to write must
    // still terminate the process.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _ = rustix::io::write(
        rustix::stdio::stderr(),
        b"memory allocation failed in native worker\n",
    );
    std::process::abort()
}

// SAFETY: every allocation/deallocation uses System with its original layout;
// failure can abort, but none of these methods may unwind. Activation never
// changes allocator ownership, including for storage allocated before activation.
unsafe impl GlobalAlloc for WorkerAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        checked(unsafe { System.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        checked(unsafe { System.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from this same System allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes: usize) -> *mut u8 {
        // SAFETY: the caller supplies the original allocation and valid new size.
        checked(unsafe { System.realloc(pointer, layout, bytes) })
    }
}

/// One-way process policy; call only after installing disposable-worker limits.
pub(super) fn enforce() {
    ENABLED.store(true, Ordering::SeqCst);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::io::{Read, Seek};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    const PROBE: &str = "TPE_ALLOCATOR_TEST_PROBE";
    const PARENT: &str = "TPE_ALLOCATOR_TEST_PARENT";
    const FAILURE: &str = "memory allocation failed in native worker";

    fn subprocess(mode: &str) -> (ExitStatus, String) {
        let capture = tempfile::tempfile().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker_allocator::tests::allocator_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE, mode)
            .env(PARENT, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(capture.try_clone().unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("allocator probe {mode} exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut capture = capture;
        capture.rewind().unwrap();
        let mut stderr = String::new();
        capture.take(16 * 1024).read_to_string(&mut stderr).unwrap();
        eprintln!("allocator-evidence mode={mode} exit={status} stderr={stderr}");
        (status, stderr)
    }

    #[test]
    fn fallible_allocations_remain_recoverable_outside_workers() {
        for mode in ["inactive-alloc", "inactive-realloc", "inactive-zeroed"] {
            let (status, stderr) = subprocess(mode);
            assert!(status.success(), "{mode}: {status}: {stderr}");
            assert!(stderr.contains("recovered allocation failure"), "{stderr}");
            assert!(!stderr.contains(FAILURE), "{stderr}");
        }
    }

    #[test]
    fn workers_abort_even_when_the_caller_uses_fallible_allocation() {
        for mode in ["active-alloc", "active-realloc", "active-zeroed"] {
            let (status, stderr) = subprocess(mode);
            assert_eq!(status.signal(), Some(6), "{mode}: {status}: {stderr}");
            assert!(stderr.contains(FAILURE), "{stderr}");
            assert!(!stderr.contains("recovered allocation failure"), "{stderr}");
        }
    }

    #[test]
    fn workers_preserve_small_allocations_and_existing_storage() {
        let (status, stderr) = subprocess("active-small");
        assert!(status.success(), "{status}: {stderr}");
        assert!(
            stderr.contains("small allocation control passed"),
            "{stderr}"
        );
        assert!(!stderr.contains(FAILURE), "{stderr}");
    }

    /// Limits and allocator policy affect only this fresh child process.
    #[test]
    #[ignore = "run only by the bounded parent probes"]
    fn allocator_probe() {
        let mode = std::env::var(PROBE).expect("allocator probe mode required");
        let parent = std::env::var(PARENT).unwrap().parse().unwrap();
        // Storage allocated before activation must still grow and free through System.
        let mut existing = vec![42_u8; 4096];
        let limits = crate::worker_limits::install(1024 * 1024 * 1024, parent).unwrap();
        let bytes = usize::try_from(limits.effective_address_space_bytes)
            .unwrap()
            .checked_add(1024 * 1024)
            .unwrap();
        // This must reach the allocator, not fail Vec's capacity-overflow check.
        let layout = Layout::array::<u8>(bytes).unwrap();
        eprintln!(
            "allocator-request mode={mode} bytes={bytes} address_limit={}",
            limits.effective_address_space_bytes
        );
        if mode.starts_with("active-") {
            enforce();
        }
        if mode == "active-small" {
            let mut fresh = Vec::<u8>::new();
            fresh.try_reserve_exact(64 * 1024).unwrap();
            fresh.extend_from_slice(b"allocation control");
            existing.try_reserve_exact(64 * 1024).unwrap();
            assert!(existing.iter().all(|byte| *byte == 42));
            assert_eq!(fresh, b"allocation control");
            // SAFETY: a nonzero valid layout; its returned allocation is freed once.
            let small = Layout::from_size_align(4096, 8).unwrap();
            let pointer = std::hint::black_box(unsafe {
                std::alloc::alloc_zeroed(std::hint::black_box(small))
            });
            assert!(!pointer.is_null());
            // SAFETY: pointer is a live, non-null 4096-byte allocation.
            assert_eq!(unsafe { *pointer }, 0);
            // SAFETY: exact layout and pointer from alloc_zeroed above.
            unsafe { std::alloc::dealloc(pointer, small) };
            eprintln!("small allocation control passed");
            return;
        }
        if mode.ends_with("zeroed") {
            // SAFETY: layout is nonzero and valid. An unexpected successful
            // allocation is freed without touching its potentially huge mapping.
            let pointer = std::hint::black_box(unsafe {
                std::alloc::alloc_zeroed(std::hint::black_box(layout))
            });
            if !pointer.is_null() {
                // SAFETY: exact pointer/layout returned above, still live.
                unsafe { std::alloc::dealloc(pointer, layout) };
                panic!("oversized zeroed allocation unexpectedly succeeded");
            }
        } else {
            let mut buffer = if mode.ends_with("realloc") {
                existing
            } else {
                Vec::<u8>::new()
            };
            let result = buffer.try_reserve_exact(std::hint::black_box(bytes - buffer.len()));
            assert!(result.is_err(), "oversized fallible allocation succeeded");
            std::hint::black_box(&buffer);
        }
        eprintln!("recovered allocation failure");
    }
}
