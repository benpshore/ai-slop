//! Process-local limits installed before a worker reads document content.
//!
//! The hard limit is virtual address space, including native allocations and
//! mappings. The requested allowance is growth above trusted startup mappings,
//! not an RSS limit: already reserved allocator arenas can become resident.
//! Both extraction and result-publication workers need this boundary. The
//! controller must not decode an unbounded result outside it.
//!
//! Linux: <https://man7.org/linux/man-pages/man2/getrlimit.2.html>.
//! Darwin checks `RLIMIT_AS` in `vm_map_enter` and rejects installation below the
//! current map size. `RLIMIT_RSS` is an alias, not a separate physical-memory cap:
//! <https://github.com/apple-oss-distributions/xnu/blob/xnu-11417.140.69/bsd/kern/kern_resource.c#L1345>
//! <https://github.com/apple-oss-distributions/xnu/blob/xnu-11417.140.69/osfmk/vm/vm_map.c#L3415>.
//!
//! Call only in a disposable worker. Lowering a hard limit is irreversible;
//! any error requires worker termination, never continuing without limits.

use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::{Context, ensure};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LimitKind {
    AddressSpaceGrowth,
}

/// What was actually installed, rather than only the requested allowance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct LimitEvidence {
    pub kind: LimitKind,
    pub startup_virtual_bytes: u64,
    pub requested_growth_bytes: u64,
    pub effective_address_space_bytes: u64,
    pub inherited_soft_bytes: Option<u64>,
    pub inherited_hard_bytes: Option<u64>,
    pub core_dump_bytes: u64,
    /// Linux SIGKILL protection also works while the worker is stopped.
    /// On macOS the controller must own kill/reap and maintain its stdin lease;
    /// an EOF watchdog cannot run while the entire process is stopped.
    pub kernel_parent_death_signal: bool,
}

/// Install and verify the process boundary before any request/PDF decoding.
///
/// `expected_parent` is the controller PID supplied at spawn. The Linux
/// parent-death signal refers to the spawning thread, which must remain alive
/// until the worker exits. The PID checks close the normal reparenting race.
/// A caller must exit on any error, including a partially installed boundary.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn install(growth_bytes: u64, expected_parent: u32) -> anyhow::Result<LimitEvidence> {
    ensure!(
        growth_bytes > 0,
        "worker address-space growth must be positive"
    );
    check_parent(expected_parent)?;
    let baseline = virtual_bytes()?;
    ensure!(baseline > 0, "worker startup virtual size is unavailable");
    let desired = baseline
        .checked_add(growth_bytes)
        .context("worker address-space limit overflow")?;
    let inherited = getrlimit(Resource::As);
    // Never relax an inherited soft limit merely because its hard limit is
    // larger. No platform-specific retry may silently widen this result.
    let effective = inherited
        .current
        .into_iter()
        .chain(inherited.maximum)
        .fold(desired, u64::min);
    ensure!(
        effective > baseline,
        "inherited address-space limit leaves no worker startup headroom"
    );

    let zero = Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    setrlimit(Resource::Core, zero).context("disabling worker core dumps")?;
    ensure!(
        getrlimit(Resource::Core) == zero,
        "worker core limit was not installed"
    );
    install_parent_protection(expected_parent)?;

    let limit = Rlimit {
        current: Some(effective),
        maximum: Some(effective),
    };
    setrlimit(Resource::As, limit).context("installing worker address-space limit")?;
    ensure!(
        getrlimit(Resource::As) == limit,
        "worker address-space limit was not installed"
    );
    check_parent(expected_parent)?;
    Ok(LimitEvidence {
        kind: LimitKind::AddressSpaceGrowth,
        startup_virtual_bytes: baseline,
        requested_growth_bytes: growth_bytes,
        effective_address_space_bytes: effective,
        inherited_soft_bytes: inherited.current,
        inherited_hard_bytes: inherited.maximum,
        core_dump_bytes: 0,
        kernel_parent_death_signal: cfg!(target_os = "linux"),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn install(_growth_bytes: u64, _expected_parent: u32) -> anyhow::Result<LimitEvidence> {
    anyhow::bail!("hard worker address-space limits require Linux or macOS")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_parent(expected_parent: u32) -> anyhow::Result<()> {
    let expected = i32::try_from(expected_parent).context("invalid controller process ID")?;
    ensure!(expected > 0, "invalid controller process ID");
    ensure!(
        rustix::process::getppid().is_some_and(|pid| pid.as_raw_pid() == expected),
        "extraction controller exited or changed before worker setup"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_parent_protection(expected_parent: u32) -> anyhow::Result<()> {
    use rustix::process::{DumpableBehavior, Signal};

    // RLIMIT_CORE=0 alone is ignored for a piped core_pattern handler. This
    // process-local switch suppresses dumps even in that configuration.
    // <https://man7.org/linux/man-pages/man5/core.5.html>
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)
        .context("disabling worker dumpability")?;
    ensure!(
        rustix::process::dumpable_behavior()? == DumpableBehavior::NotDumpable,
        "worker dumpability was not disabled"
    );
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
        .context("installing worker parent-death signal")?;
    ensure!(
        rustix::process::parent_process_death_signal()? == Some(Signal::KILL),
        "worker parent-death signal was not installed"
    );
    check_parent(expected_parent)
}

#[cfg(target_os = "macos")]
fn install_parent_protection(expected_parent: u32) -> anyhow::Result<()> {
    // There is no portable Darwin counterpart to PR_SET_PDEATHSIG here.
    // The worker protocol must additionally maintain its controller lease.
    check_parent(expected_parent)
}

#[cfg(target_os = "linux")]
fn virtual_bytes() -> anyhow::Result<u64> {
    use std::io::Read;

    // Only the first numeric field is needed; never allocate from procfs data.
    let mut buffer = [0_u8; 256];
    let count = std::fs::File::open("/proc/self/statm")
        .context("opening worker virtual-memory accounting")?
        .read(&mut buffer)
        .context("reading worker virtual-memory accounting")?;
    let end = buffer[..count]
        .iter()
        .position(u8::is_ascii_whitespace)
        .context("missing worker virtual-memory page count")?;
    let pages: u64 = std::str::from_utf8(&buffer[..end])?
        .parse()
        .context("invalid worker virtual-memory page count")?;
    let page_size = u64::try_from(rustix::param::page_size())?;
    ensure!(page_size > 0, "worker page size is unavailable");
    pages
        .checked_mul(page_size)
        .context("worker virtual-memory accounting overflow")
}

#[cfg(target_os = "macos")]
fn virtual_bytes() -> anyhow::Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>())?;
    let pid = i32::try_from(std::process::id())?;
    // SAFETY: the buffer is correctly aligned and exactly size bytes long;
    // proc_pidinfo receives its exclusive writable pointer and retains none.
    // We read fields only after the exact full-structure return is confirmed.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    ensure!(
        written == size,
        "worker virtual-memory accounting returned {written} bytes, expected {size}"
    );
    // SAFETY: the successful exact-size call initialized the full structure.
    let info = unsafe { info.assume_init() };
    // XNU reports vm_map_adjusted_size. Exotic/transformed processes may have
    // reserved regions subtracted while RLIMIT_AS checks the full map size.
    // Installation must therefore fail closed rather than increasing the cap.
    // <https://github.com/apple-oss-distributions/xnu/blob/xnu-11417.140.69/osfmk/kern/bsd_kern.c#L1010>
    Ok(info.pti_virtual_size)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    const MIB: u64 = 1024 * 1024;
    const PROBE_ENV: &str = "TPE_WORKER_LIMIT_TEST_PROBE";
    const PARENT_ENV: &str = "TPE_WORKER_LIMIT_TEST_PARENT";

    #[test]
    fn os_address_space_boundary_rejects_mapping_and_heap_growth() {
        subprocess("allocation");
    }

    #[test]
    fn os_address_space_boundary_preserves_stricter_inherited_limits() {
        subprocess("inherited");
    }

    #[test]
    fn os_address_space_boundary_fails_closed_on_invalid_setup() {
        subprocess("invalid");
    }

    fn subprocess(case: &str) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker_limits::tests::os_limit_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, case)
            .env(PARENT_ENV, std::process::id().to_string())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "worker limit probe {case} exited {status}"
                );
                break;
            }
            if started.elapsed() > Duration::from_secs(15) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("worker limit probe {case} exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// This process alone receives irreversible limits, never the test runner.
    #[test]
    #[ignore = "invoked in a disposable subprocess by the boundary tests"]
    fn os_limit_probe() {
        let Ok(case) = std::env::var(PROBE_ENV) else {
            return;
        };
        let parent = std::env::var(PARENT_ENV).unwrap().parse().unwrap();
        match case.as_str() {
            "allocation" => {
                let evidence = install(32 * MIB, parent).unwrap();
                assert_eq!(evidence.kind, LimitKind::AddressSpaceGrowth);
                assert!(
                    evidence.effective_address_space_bytes
                        <= evidence.startup_virtual_bytes + 32 * MIB
                );
                assert_eq!(getrlimit(Resource::Core).maximum, Some(0));
                mapping_probe(MIB, true);
                mapping_probe(128 * MIB, false);
                let mut heap = Vec::<u8>::new();
                // Bigger than the entire effective address space, not just
                // the allowance: allocator reservations cannot satisfy this.
                let excessive = usize::try_from(evidence.effective_address_space_bytes).unwrap();
                assert!(heap.try_reserve_exact(excessive).is_err());
                #[cfg(target_os = "linux")]
                {
                    assert!(evidence.kernel_parent_death_signal);
                    assert_eq!(
                        rustix::process::parent_process_death_signal().unwrap(),
                        Some(rustix::process::Signal::KILL)
                    );
                    assert_eq!(
                        rustix::process::dumpable_behavior().unwrap(),
                        rustix::process::DumpableBehavior::NotDumpable
                    );
                }
                println!(
                    "worker-limit evidence: {}",
                    serde_json::to_string(&evidence).unwrap()
                );
            }
            "inherited" => {
                let baseline = virtual_bytes().unwrap();
                let lower = baseline.checked_add(64 * MIB).unwrap();
                let higher = baseline.checked_add(96 * MIB).unwrap();
                setrlimit(
                    Resource::As,
                    Rlimit {
                        current: Some(lower),
                        maximum: Some(higher),
                    },
                )
                .unwrap();
                let evidence = install(256 * MIB, parent).unwrap();
                assert_eq!(evidence.inherited_soft_bytes, Some(lower));
                assert_eq!(evidence.inherited_hard_bytes, Some(higher));
                assert_eq!(evidence.effective_address_space_bytes, lower);
                assert_eq!(
                    getrlimit(Resource::As),
                    Rlimit {
                        current: Some(lower),
                        maximum: Some(lower)
                    }
                );
                assert!(
                    setrlimit(
                        Resource::As,
                        Rlimit {
                            current: Some(higher),
                            maximum: Some(higher)
                        }
                    )
                    .is_err()
                );
                mapping_probe(MIB, true);
                mapping_probe(128 * MIB, false);
            }
            "invalid" => {
                let before = getrlimit(Resource::As);
                assert!(install(0, parent).is_err());
                assert!(install(u64::MAX, parent).is_err());
                assert!(install(MIB, 0).is_err());
                assert!(install(MIB, std::process::id()).is_err());
                assert_eq!(getrlimit(Resource::As), before);
            }
            _ => panic!("unknown worker limit probe {case}"),
        }
    }

    fn mapping_probe(bytes: u64, should_succeed: bool) {
        use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

        let length = usize::try_from(bytes).unwrap();
        // SAFETY: null requests a fresh kernel-chosen address. No references
        // alias this private mapping; the returned region is always unmapped.
        let mapping = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                length,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
            )
        };
        match mapping {
            Ok(pointer) => {
                if should_succeed {
                    // SAFETY: these two bytes are inside the fresh nonempty
                    // writable mapping. Volatile writes prove usable pages.
                    unsafe {
                        pointer.cast::<u8>().write_volatile(1);
                        pointer.cast::<u8>().add(length - 1).write_volatile(1);
                    }
                }
                // SAFETY: this is the untouched base and length from mmap;
                // no references to the mapping exist when it is released.
                unsafe { munmap(pointer, length).unwrap() };
                assert!(
                    should_succeed,
                    "OS accepted a mapping over the address-space limit"
                );
            }
            Err(error) => {
                assert!(!should_succeed, "small mapping failed: {error}");
                assert_eq!(error, rustix::io::Errno::NOMEM);
            }
        }
    }
}
