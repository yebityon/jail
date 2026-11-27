//! Host-side PTY mediation. A typed /login is answered by jail and never reaches
//! the agent CLI: sign-in happens only on the Mac.
use crate::sys;
use anyhow::{Context, Result};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use std::{
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

struct Terminal(libc::termios);
impl Terminal {
    fn new() -> Result<Self> {
        Ok(Self(sys::terminal_mode(std::io::stdin().as_fd())?))
    }
    fn raw(&self) -> Result<()> {
        self.set(&sys::raw_mode(self.0))
    }
    fn restore(&self) -> Result<()> {
        self.set(&self.0)
    }
    fn set(&self, mode: &libc::termios) -> Result<()> {
        Ok(sys::set_terminal_mode(std::io::stdin().as_fd(), mode)?)
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
    let mut size = sys::window_size(std::io::stdin().as_fd());
    let (mut master, slave) = sys::open_pty(&terminal.0, &size)?;
    // Do not leak the master into either the container client or OAuth CLI.
    let _ = sys::close_on_exec(master.as_fd());
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    sys::controlling_terminal(command);
    let mut child = Child(
        command
            .spawn()
            .context("could not start interactive container client")?,
    );
    let pid = sys::Pid::of(&child.0)?;
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
            let _ = pid.signal_group(SIGTERM);
            return Ok(130);
        }
        let new_size = sys::window_size(std::io::stdin().as_fd());
        if (size.ws_col, size.ws_row) != (new_size.ws_col, new_size.ws_row) {
            size = new_size;
            let _ = sys::set_window_size(master.as_fd(), &size);
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
        if let Err(e) = sys::poll(&mut polls, 100) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
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
