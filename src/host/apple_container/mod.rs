//! Rust interface to Apple's `container` command. It knows the command's
//! arguments and JSON layouts, nothing about jail: callers choose the names,
//! labels and options of containers, volumes and images.
mod container;
mod image;
mod volume;

pub use container::{Container, Run};
pub use image::Image;
pub use volume::Volume;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    ffi::{OsStr, OsString},
    process::{Command, Stdio},
};

/// Runs one `container` executable.
pub struct Client {
    bin: OsString,
}
impl Client {
    pub fn new(bin: impl Into<OsString>) -> Self {
        Self { bin: bin.into() }
    }
    fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut command = Command::new(&self.bin);
        command.args(args);
        command
    }
    /// Stdout of a successful run.
    fn capture<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>> {
        let output = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .context("Apple container not found; install it")?;
        if !output.status.success() {
            // Do not include argv: credentials or environment values can be present.
            bail!(
                "container operation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }
    fn checked<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<()> {
        self.capture(args)?;
        Ok(())
    }
    /// Whether the run succeeds; its output is discarded.
    fn succeeds<S: AsRef<OsStr>>(&self, args: &[S]) -> bool {
        self.command(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    /// The entries a `list --format json` command prints.
    fn entries<S: AsRef<OsStr>>(&self, args: &[S], kind: &str) -> Result<Vec<Value>> {
        match serde_json::from_slice(&self.capture(args)?)? {
            Value::Array(entries) => Ok(entries),
            _ => bail!("unexpected {kind} list JSON"),
        }
    }
    pub fn service_running(&self) -> bool {
        self.command(&["system", "status"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    pub fn start_service(&self) -> Result<()> {
        self.checked(&["system", "start", "--enable-kernel-install"])
    }
}
