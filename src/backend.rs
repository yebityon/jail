use crate::{exit_code, project::Record};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
};

// Container JSON layouts differ between backend versions; read them only here.
pub fn container_id(entry: &Value) -> Option<&str> {
    entry
        .get("id")
        .or_else(|| entry.pointer("/configuration/id"))
        .and_then(Value::as_str)
}
/// Whether the ownership label names `root`. Accepts a list entry or
/// `inspect` output, which wraps the container in an array.
pub fn labeled_for(value: &Value, root: &Path) -> bool {
    let entry = value.as_array().and_then(|a| a.first()).unwrap_or(value);
    entry
        .pointer("/configuration/labels/io.jail.project")
        .or_else(|| entry.pointer("/labels/io.jail.project"))
        .and_then(Value::as_str)
        == Some(root.to_string_lossy().as_ref())
}
pub fn container_state(entry: &Value) -> Option<String> {
    entry
        .pointer("/status/state")
        .or_else(|| entry.get("status"))
        .or_else(|| entry.get("state"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

pub struct Backend {
    bin: OsString,
}
impl Backend {
    pub fn new() -> Self {
        Self {
            bin: std::env::var_os("JAIL_CONTAINER_BIN").unwrap_or("container".into()),
        }
    }
    pub fn command<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Command {
        let mut command = Command::new(&self.bin);
        command.args(args);
        command
    }
    pub fn capture<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<Vec<u8>> {
        let output = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .context("Apple container not found; install it and run jail doctor")?;
        if !output.status.success() {
            // Do not include argv: credentials or environment values can be present.
            bail!(
                "container operation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }
    pub fn checked<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<()> {
        self.capture(args)?;
        Ok(())
    }
    pub fn interactive(&self, args: &[&str]) -> Result<i32> {
        self.interactive_owned(args)
    }
    pub fn interactive_owned<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<i32> {
        Ok(exit_code(
            self.command(args)
                .status()
                .context("could not run Apple container")?,
        ))
    }
    pub fn ensure_service(&self) -> Result<()> {
        if self
            .command(&["system", "status"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        eprintln!("jail: starting Apple container service");
        self.checked(&["system", "start", "--enable-kernel-install"])
    }
    pub fn list(&self) -> Result<Vec<Value>> {
        let value: Value =
            serde_json::from_slice(&self.capture(&["list", "--all", "--format", "json"])?)?;
        value
            .as_array()
            .cloned()
            .context("unexpected container list JSON")
    }
    pub fn owned_state(&self, record: &Record) -> Result<Option<String>> {
        for entry in self.list()? {
            if container_id(&entry) != Some(&record.id) {
                continue;
            }
            if !labeled_for(&entry, &record.root) {
                // Some list versions omit labels, so inspect the exact container.
                let inspected: Value =
                    serde_json::from_slice(&self.capture(&["inspect", &record.id])?)?;
                if !labeled_for(&inspected, &record.root) {
                    bail!(
                        "refusing to use container {}: ownership label mismatch",
                        record.id
                    );
                }
            }
            return container_state(&entry)
                .map(Some)
                .context("container state is missing");
        }
        Ok(None)
    }
    pub fn require_owned(&self, record: &Record) -> Result<()> {
        if self.owned_state(record)?.is_none() {
            bail!("container {} does not exist", record.id);
        }
        Ok(())
    }
    pub fn doctor(&self) -> Result<i32> {
        println!(
            "host: {} / {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
            bail!("Apple container requires an Apple silicon Mac");
        }
        let version = self.capture(&["--version"])?;
        println!("{}", String::from_utf8_lossy(&version).trim());
        let status = self.interactive(&["system", "status"])?;
        if status != 0 {
            println!("service is stopped; jail will start it on launch");
        }
        println!("host authentication will be imported on first launch; no credential contents are printed");
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn both_container_json_layouts_are_read() {
        let root = Path::new("/work/app");
        let flat =
            json!({"id": "a", "labels": {"io.jail.project": "/work/app"}, "status": "Running"});
        let nested = json!({"configuration": {"id": "b", "labels": {"io.jail.project": "/work/app"}},
            "status": {"state": "stopped"}});
        assert_eq!(container_id(&flat), Some("a"));
        assert_eq!(container_id(&nested), Some("b"));
        assert!(labeled_for(&flat, root) && labeled_for(&nested, root));
        assert!(labeled_for(&json!([nested]), root));
        assert!(!labeled_for(&flat, Path::new("/work/other")));
        assert!(!labeled_for(&json!([]), root));
        assert_eq!(container_state(&flat).as_deref(), Some("running"));
        assert_eq!(container_state(&nested).as_deref(), Some("stopped"));
        assert_eq!(
            container_state(&json!({"state": "Exited"})).as_deref(),
            Some("exited")
        );
        assert_eq!(container_state(&json!({"id": "c"})), None);
    }
}
