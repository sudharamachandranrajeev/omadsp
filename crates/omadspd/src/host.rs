//! The filter host: a `pipewire -c host.conf` child process (ADR-2).
//!
//! - The child gets `PR_SET_PDEATHSIG = SIGTERM`, so a dead daemon never leaves an unmanaged
//!   filter behind; WirePlumber then links streams straight to the device.
//! - Exits are noticed through a pidfd on the main loop (no SIGCHLD handler, no polling).
//! - [`RestartPolicy`]: backoff 250 ms → 5 s; five crashes within 60 s and it stays down.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use omadsp_core::graph::{Options, host_conf};
use omadsp_core::managed_file;
use omadsp_core::model::Preset;

pub const BACKOFF_MIN: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);
pub const CRASH_WINDOW: Duration = Duration::from_secs(60);
pub const MAX_CRASHES: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    RestartAfter(Duration),
    GiveUp,
}

#[derive(Debug, Default)]
pub struct RestartPolicy {
    crashes: VecDeque<Instant>,
    consecutive: u32,
}

impl RestartPolicy {
    /// Call when the host exited without being asked to. `ran_for` is how long it had been up.
    pub fn on_crash(&mut self, now: Instant, ran_for: Duration) -> Decision {
        if ran_for >= CRASH_WINDOW {
            self.consecutive = 0;
        }
        self.crashes.push_back(now);
        while self.crashes.front().is_some_and(|&t| now.duration_since(t) > CRASH_WINDOW) {
            self.crashes.pop_front();
        }
        if self.crashes.len() >= MAX_CRASHES {
            return Decision::GiveUp;
        }
        let d = BACKOFF_MIN.saturating_mul(1 << self.consecutive.min(8));
        self.consecutive += 1;
        Decision::RestartAfter(d.min(BACKOFF_MAX))
    }

    /// Forget past crashes (the user asked for a fresh start).
    pub fn reset(&mut self) {
        *self = RestartPolicy::default();
    }
}

/// Writes the host config for `preset` to `path` (managed file, mode 0600).
pub fn write_conf(path: &Path, preset: &Preset, opts: &Options) -> io::Result<()> {
    managed_file::write(path, &host_conf(preset, opts), 0o600)
}

/// A running host process.
pub struct HostProcess {
    child: Child,
    pidfd: OwnedFd,
    pub started: Instant,
}

/// The pidfd becomes readable when the process exits.
pub struct PidFd(RawFd);

impl AsRawFd for PidFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl HostProcess {
    /// Runs `pipewire -c <conf>` (`conf` must be absolute: relative names are looked up in
    /// PipeWire's config directories).
    pub fn spawn(conf: &Path) -> io::Result<HostProcess> {
        if !conf.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "host config path must be absolute"));
        }
        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(conf);
        Self::spawn_cmd(cmd)
    }

    pub fn spawn_cmd(mut cmd: Command) -> io::Result<HostProcess> {
        let parent = std::process::id() as libc::pid_t;
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
        // SAFETY: only async-signal-safe calls (prctl, getppid) between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // The parent may have died before prctl took effect.
                if libc::getppid() != parent {
                    return Err(io::Error::other("parent exited"));
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // SAFETY: plain syscall; a non-negative result is a new fd we own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        // SAFETY: fd is valid and owned by nobody else.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        Ok(HostProcess { child, pidfd, started: Instant::now() })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A handle for the main loop's IO watch. Valid while `self` lives.
    pub fn pidfd(&self) -> PidFd {
        PidFd(self.pidfd.as_raw_fd())
    }

    /// The exit status, if the process has exited (reaps it).
    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// SIGTERM, then SIGKILL after `grace`. Always reaps.
    pub fn terminate(mut self, grace: Duration) -> Option<ExitStatus> {
        if let Some(s) = self.try_exit() {
            return Some(s);
        }
        // SAFETY: signalling our own, not yet reaped child.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let t = Instant::now();
        while t.elapsed() < grace {
            if let Some(s) = self.try_exit() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        self.child.wait().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut p = RestartPolicy::default();
        let t0 = Instant::now();
        let short = Duration::from_secs(1);
        let mut waits = Vec::new();
        for i in 0..4 {
            match p.on_crash(t0 + Duration::from_secs(20 * i), short) {
                Decision::RestartAfter(d) => waits.push(d.as_millis()),
                Decision::GiveUp => panic!("gave up early"),
            }
        }
        assert_eq!(waits, [250, 500, 1000, 2000]);
        // Old crashes leave the window: no give-up, and the delay keeps growing to the cap.
        let d = p.on_crash(t0 + Duration::from_secs(100), short);
        assert_eq!(d, Decision::RestartAfter(Duration::from_secs(4)));
        let d = p.on_crash(t0 + Duration::from_secs(130), short);
        assert_eq!(d, Decision::RestartAfter(BACKOFF_MAX));
    }

    #[test]
    fn five_crashes_in_a_minute_give_up() {
        let mut p = RestartPolicy::default();
        let t0 = Instant::now();
        for i in 0..4 {
            assert!(matches!(p.on_crash(t0 + Duration::from_secs(i), Duration::ZERO), Decision::RestartAfter(_)));
        }
        assert_eq!(p.on_crash(t0 + Duration::from_secs(5), Duration::ZERO), Decision::GiveUp);
        p.reset();
        assert_eq!(p.on_crash(t0 + Duration::from_secs(6), Duration::ZERO), Decision::RestartAfter(BACKOFF_MIN));
    }

    #[test]
    fn a_long_healthy_run_resets_the_backoff() {
        let mut p = RestartPolicy::default();
        let t0 = Instant::now();
        p.on_crash(t0, Duration::ZERO);
        p.on_crash(t0 + Duration::from_secs(1), Duration::ZERO);
        let d = p.on_crash(t0 + Duration::from_secs(200), Duration::from_secs(199));
        assert_eq!(d, Decision::RestartAfter(BACKOFF_MIN));
    }

    #[test]
    fn pidfd_reports_exit_and_terminate_reaps() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let mut h = HostProcess::spawn_cmd(cmd).unwrap();
        assert!(h.try_exit().is_none());
        let mut pfd = libc::pollfd { fd: h.pidfd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: polling one valid fd.
        assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 0, "not readable while running");
        // SAFETY: our child.
        unsafe { libc::kill(h.pid() as libc::pid_t, libc::SIGKILL) };
        // SAFETY: as above.
        assert_eq!(unsafe { libc::poll(&mut pfd, 1, 2000) }, 1, "readable after exit");
        let s = h.terminate(Duration::from_millis(100)).unwrap();
        assert!(!s.success());
    }

    #[test]
    fn relative_conf_is_refused() {
        assert!(HostProcess::spawn(Path::new("host.conf")).is_err());
    }
}
