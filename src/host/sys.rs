//! The host's calls into libc. Each `unsafe` block states why it is sound.
use anyhow::{bail, Result};
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command},
};

pub fn uid() -> libc::uid_t {
    // SAFETY: getuid takes no arguments and always succeeds.
    unsafe { libc::getuid() }
}
pub fn gid() -> libc::gid_t {
    // SAFETY: getgid takes no arguments and always succeeds.
    unsafe { libc::getgid() }
}
/// Keeps `fd` open in programs that this process executes.
pub fn inherit_on_exec(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `fd` is borrowed, so it stays open during the call, and
    // F_GETFD takes no pointer.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    set_fd_flags(fd, flags & !libc::FD_CLOEXEC)
}
/// Closes `fd` in programs that this process executes.
pub fn close_on_exec(fd: BorrowedFd<'_>) -> io::Result<()> {
    set_fd_flags(fd, libc::FD_CLOEXEC)
}
fn set_fd_flags(fd: BorrowedFd<'_>, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is borrowed, so it stays open during the call, and
    // F_SETFD takes an integer.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A child's process ID. It is never 0 or negative, which `kill` reads as
/// process groups.
#[derive(Clone, Copy)]
pub struct Pid(libc::pid_t);
impl Pid {
    pub fn of(child: &Child) -> Result<Self> {
        match libc::pid_t::try_from(child.id()) {
            Ok(pid) if pid > 0 => Ok(Self(pid)),
            _ => bail!("child process ID {} is out of range", child.id()),
        }
    }
    pub fn signal(self, signal: libc::c_int) -> io::Result<()> {
        kill(self.0, signal)
    }
    /// Signals the process group that this process leads.
    pub fn signal_group(self, signal: libc::c_int) -> io::Result<()> {
        kill(-self.0, signal)
    }
}
fn kill(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: kill takes only integers.
    if unsafe { libc::kill(pid, signal) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `seconds` since the Unix epoch, broken down in the local time zone.
pub fn local_time(seconds: libc::time_t) -> Option<libc::tm> {
    // SAFETY: tm holds integers and, on some platforms, a pointer; all-zero
    // bytes are a valid value for each.
    let mut date: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers come from live references, and localtime_r
    // writes only to `date`.
    if unsafe { libc::localtime_r(&seconds, &mut date) }.is_null() {
        return None;
    }
    Some(date)
}

pub fn terminal_mode(fd: BorrowedFd<'_>) -> io::Result<libc::termios> {
    // SAFETY: termios holds only integers, for which zero is valid.
    let mut mode: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr writes one termios to `mode`, a live local.
    if unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}
pub fn set_terminal_mode(fd: BorrowedFd<'_>, mode: &libc::termios) -> io::Result<()> {
    // SAFETY: tcsetattr only reads `mode`, a live reference.
    if unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
/// `mode` with raw input and output.
pub fn raw_mode(mut mode: libc::termios) -> libc::termios {
    // SAFETY: cfmakeraw changes only fields of `mode`, a live local.
    unsafe { libc::cfmakeraw(&mut mode) };
    mode
}
/// The terminal's window size, or zeros when `fd` has none.
pub fn window_size(fd: BorrowedFd<'_>) -> libc::winsize {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize to `size`, a live local.
    unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    size
}
pub fn set_window_size(fd: BorrowedFd<'_>, size: &libc::winsize) -> io::Result<()> {
    // SAFETY: TIOCSWINSZ reads one winsize from `size`, a live reference.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
/// A new pseudo-terminal with `mode` and `size`, as (master, slave).
pub fn open_pty(mode: &libc::termios, size: &libc::winsize) -> io::Result<(File, File)> {
    let (mut master, mut slave) = (-1, -1);
    let (mut mode, mut size) = (*mode, *size);
    // SAFETY: openpty writes one descriptor each to `master` and `slave` and
    // reads `mode` and `size`, all live locals. A null name is allowed.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            &mut mode,
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are open descriptors that nothing
    // else owns.
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}
/// Starts `command` in a new session whose controlling terminal is its stdin.
pub fn controlling_terminal(command: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec. It calls only
    // setsid and ioctl, which are async-signal-safe, and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
/// Waits up to `timeout` milliseconds for an event on any of `fds`.
pub fn poll(fds: &mut [libc::pollfd], timeout: libc::c_int) -> io::Result<()> {
    let count = libc::nfds_t::try_from(fds.len())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: the pointer and count describe `fds`, a live slice that poll
    // may write.
    if unsafe { libc::poll(fds.as_mut_ptr(), count, timeout) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
