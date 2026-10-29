//! Resource politeness: a single-run lock, low CPU/IO priority, and per-run
//! budgets so a nightly job can never become the reason a live stream stutters.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Held for the lifetime of a run. Second instance fails fast instead of
/// fighting over the same files.
pub struct RunLock {
    _file: File,
    path: std::path::PathBuf,
}

impl RunLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            bail!(
                "another mediaopt run holds {}; wait for it to finish or stop it first",
                path.display()
            );
        }
        Ok(RunLock {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Default, Clone)]
pub struct PriorityReport {
    pub nice: Option<i32>,
    pub idle_io: bool,
}

/// Drop our own scheduling priority. Doing it in-process means it applies to
/// every encoder subprocess too, and does not depend on how we were started.
pub fn apply_low_priority() -> PriorityReport {
    let mut report = PriorityReport::default();
    // SAFETY: setpriority on our own process.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };
    if rc == 0 {
        // Read it back: on macOS setpriority can succeed without effect.
        let now = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let nice = if now == -1 {
            None
        } else {
            Some(now.clamp(-20, 19))
        };
        report.nice = nice;
    }
    #[cfg(target_os = "linux")]
    {
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        let prio = IOPRIO_CLASS_IDLE << 13;
        // SAFETY: ioprio_set on our own process.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0 as libc::c_int,
                prio,
            )
        };
        report.idle_io = rc == 0;
    }
    report
}

/// Bytes available on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Result<u64> {
    let c_path =
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).context("path contains NUL")?;
    // SAFETY: statvfs into a zeroed struct we own.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", path.display()));
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Per-run limits: wall clock, bytes written, files changed.
pub struct Budget {
    pub max_bytes: u64,
    pub max_files: u64,
    deadline: Option<Instant>,
    pub bytes_done: u64,
    pub files_done: u64,
    /// Files we actually encoded, whether or not the result was kept. The file
    /// budget counts attempts, not successes: an encode that gets rejected still
    /// costs the CPU we promised not to spend.
    pub attempts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Keep going.
    Continue,
    /// Budget or deadline reached; finish the file in flight and stop.
    Exhausted(&'static str),
}

impl Budget {
    pub fn new(max_bytes: u64, max_files: u64, max_runtime: Option<Duration>) -> Self {
        Budget {
            max_bytes,
            max_files,
            deadline: max_runtime.map(|d| Instant::now() + d),
            bytes_done: 0,
            files_done: 0,
            attempts: 0,
        }
    }

    /// Re-apply the byte and file limits for the root that is about to run,
    /// leaving the counters and the deadline alone.
    ///
    /// A root may carry a tighter allowance than the run, but the wall clock is
    /// a property of the run, not of a root: five roots must not turn one
    /// `max_runtime` into five. The counters are equally part of the run, so a
    /// global `max_files_per_run` caps the total rather than each root.
    pub fn refit(&mut self, max_bytes: u64, max_files: u64) {
        self.max_bytes = max_bytes;
        self.max_files = max_files;
    }

    pub fn check(&self) -> Stop {
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return Stop::Exhausted("max_runtime");
            }
        }
        if self.max_files > 0 && self.attempts >= self.max_files {
            return Stop::Exhausted("max_files_per_run");
        }
        if self.max_bytes > 0 && self.bytes_done >= self.max_bytes {
            return Stop::Exhausted("max_bytes_per_run");
        }
        Stop::Continue
    }

    /// Record an encode attempt (before the result is known).
    pub fn attempted(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Record a kept result.
    pub fn spent(&mut self, bytes: u64) {
        self.bytes_done = self.bytes_done.saturating_add(bytes);
        self.files_done = self.files_done.saturating_add(1);
    }
}

/// Format a byte count the way an operator thinks about it.
pub fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.2} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.2} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let first = RunLock::acquire(&path).unwrap();
        assert!(RunLock::acquire(&path).is_err());
        drop(first);
        assert!(RunLock::acquire(&path).is_ok(), "lock must release on drop");
    }

    #[test]
    fn budget_stops_on_files_and_bytes() {
        let mut b = Budget::new(1000, 0, None);
        assert_eq!(b.check(), Stop::Continue);
        b.spent(600);
        assert_eq!(b.check(), Stop::Continue);
        b.spent(600);
        assert_eq!(b.check(), Stop::Exhausted("max_bytes_per_run"));

        let mut b = Budget::new(0, 1, None);
        b.attempted();
        assert_eq!(b.check(), Stop::Exhausted("max_files_per_run"));
    }

    #[test]
    fn budget_stops_on_deadline() {
        let b = Budget::new(0, 0, Some(Duration::from_millis(0)));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(b.check(), Stop::Exhausted("max_runtime"));
    }

    #[test]
    fn refit_keeps_the_run_deadline() {
        let mut b = Budget::new(0, 0, Some(Duration::from_millis(0)));
        std::thread::sleep(Duration::from_millis(2));
        // The next root rebinds the limits; rebuilding the budget instead would
        // restart the clock and give every root its own wall clock.
        b.refit(10_000, 0);
        assert_eq!(b.check(), Stop::Exhausted("max_runtime"));
    }

    #[test]
    fn refit_keeps_the_run_totals() {
        let mut b = Budget::new(0, 0, None);
        b.attempted();
        b.attempted();
        // `max_files_per_run` counts attempts per run, so a root switch must not
        // hand the next root a fresh allowance.
        b.refit(0, 2);
        assert_eq!(b.check(), Stop::Exhausted("max_files_per_run"));
    }

    #[test]
    fn free_space_is_reported() {
        let free = free_bytes(Path::new("/")).unwrap();
        assert!(free > 0);
    }

    #[test]
    fn human_readable_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.00 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }
}
