//! Host-side PTY mediation. A typed /login is answered by jail and never reaches
//! the agent CLI: sign-in happens only on the Mac.
use anyhow::{Context, Result};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::{
        io::{AsRawFd, FromRawFd},
        process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

struct Terminal(libc::termios);
impl Terminal {
    fn new() -> Result<Self> {
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(saved))
    }
    fn raw(&self) -> Result<()> {
        let mut mode = self.0;
        unsafe { libc::cfmakeraw(&mut mode) };
        self.set(&mode)
    }
    fn restore(&self) -> Result<()> {
        self.set(&self.0)
    }
    fn set(&self, mode: &libc::termios) -> Result<()> {
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, mode) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Track only an unambiguously typed command. Pasted commands and cursor edits
/// remain native input; never interpret text from the agent's output as a login.
#[derive(Default)]
struct Input {
    line: Vec<u8>,
    uncertain: bool,
    escape: Vec<u8>,
    paste: bool,
}
impl Input {
    fn byte(&mut self, byte: u8) -> bool {
        if byte == 27 || !self.escape.is_empty() {
            self.escape.push(byte);
            if self.escape == b"\x1b[200~" {
                self.paste = true;
                self.escape.clear();
                self.uncertain = true;
            } else if self.escape == b"\x1b[201~" {
                self.paste = false;
                self.escape.clear();
            } else if self.escape.len() > 1 && (byte.is_ascii_alphabetic() || byte == b'~') {
                let response = self.escape.starts_with(b"\x1b[") && matches!(byte, b'R' | b'c');
                self.escape.clear();
                if !response {
                    self.uncertain = true;
                }
            } else if self.escape.len() > 16 {
                self.escape.clear();
                self.uncertain = true;
            }
            return false;
        }
        if self.paste {
            return false;
        }
        match byte {
            b'\r' | b'\n' => {
                let login = !self.uncertain && self.line == b"/login";
                self.line.clear();
                self.uncertain = false;
                login
            }
            3 | 21 => {
                self.line.clear();
                self.uncertain = false;
                false
            }
            8 | 127 => {
                self.line.pop();
                false
            }
            b'\t'
                if !self.uncertain
                    && [b"/log".as_slice(), b"/logi", b"/login"]
                        .contains(&self.line.as_slice()) =>
            {
                self.line = b"/login".to_vec();
                false
            }
            32..=126 if self.line.len() < 1024 => {
                self.line.push(byte);
                false
            }
            _ => {
                self.uncertain = true;
                false
            }
        }
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn run(command: &mut Command, mut login: impl FnMut() -> Result<()>) -> Result<i32> {
    let terminal = Terminal::new()?;
    let mut master = -1;
    let mut slave = -1;
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let mut slave_mode = terminal.0;
    unsafe {
        libc::ioctl(0, libc::TIOCGWINSZ, &mut size);
    }
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            &mut slave_mode,
            &mut size,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    // Do not leak the master into either the container client or OAuth CLI.
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = Child(
        command
            .spawn()
            .context("could not start interactive container client")?,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut handlers = Vec::new();
    for signal in [SIGHUP, SIGTERM] {
        handlers.push(signal_hook::flag::register(signal, stop.clone())?);
    }
    handlers.push(signal_hook::flag::register(SIGINT, interrupt.clone())?);
    struct Handlers(Vec<signal_hook::SigId>);
    impl Drop for Handlers {
        fn drop(&mut self) {
            for id in &self.0 {
                signal_hook::low_level::unregister(*id);
            }
        }
    }
    let _handlers = Handlers(handlers);
    terminal.raw()?;
    let mut input = Input::default();
    let mut buffer = [0; 16_384];
    let mut output = std::io::stdout();
    loop {
        if interrupt.swap(false, Ordering::Relaxed) {
            master.write_all(&[3])?;
        }
        if stop.load(Ordering::Relaxed) {
            unsafe {
                libc::kill(-(child.0.id() as i32), SIGTERM);
            }
            return Ok(130);
        }
        let mut new_size: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe {
            libc::ioctl(0, libc::TIOCGWINSZ, &mut new_size);
        }
        if (size.ws_col, size.ws_row) != (new_size.ws_col, new_size.ws_row) {
            size = new_size;
            unsafe {
                libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size);
            }
        }
        let mut polls = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), 2, 100) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(std::io::Error::last_os_error().into());
        }
        if polls[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    output.write_all(&buffer[..n])?;
                    output.flush()?;
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => return Err(e.into()),
            }
        }
        if polls[0].revents & libc::POLLIN != 0 {
            let n = std::io::stdin().read(&mut buffer)?;
            if n == 0 {
                break;
            }
            for &byte in &buffer[..n] {
                if input.byte(byte) {
                    // Kill the native prompt's /login without submitting it.
                    master.write_all(&[21])?;
                    output.write_all(b"\x1b[?2004l\r\n")?;
                    output.flush()?;
                    terminal.restore()?;
                    if let Err(e) = login() {
                        eprintln!("jail: {e:#}; session is still running");
                    }
                    interrupt.store(false, Ordering::Relaxed);
                    terminal.raw()?;
                    output.write_all(b"\x1b[?2004h")?;
                    output.flush()?;
                    // Both supported TUIs use Ctrl+L to redraw after jail's message.
                    master.write_all(&[12])?;
                } else {
                    master.write_all(&[byte])?;
                }
            }
        }
    }
    drop(master);
    Ok(crate::exit_code(child.0.wait()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn logins(bytes: &[u8]) -> usize {
        let mut input = Input::default();
        bytes.iter().filter(|&&b| input.byte(b)).count()
    }
    #[test]
    fn only_explicit_commands_trigger_login() {
        assert_eq!(logins(b"/login\r/login\n"), 2);
        assert_eq!(logins(b"/logx\x7fi\x7fin\r"), 1);
        assert_eq!(logins(b"/log\t\r"), 1);
        for text in [
            b"please /login\r".as_slice(),
            b"/login extra\r",
            b"/login\x1b[D\r",
            b"\x1b[200~/login\n\x1b[201~\r",
        ] {
            assert_eq!(logins(text), 0);
        }
        assert_eq!(logins(b"text\x15/login\r"), 1);
        assert_eq!(logins(b"\x1b[24;1R/login\r"), 1);
    }
}
