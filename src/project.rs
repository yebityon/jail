use anyhow::{bail, Context, Result};
use jail::{config::validate_mount_path, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub root: PathBuf,
    pub spec: String,
    pub image: String,
    pub volume: String,
    #[serde(default)]
    pub ports: Vec<Port>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Port {
    pub host: u16,
    pub container: u16,
}
impl Port {
    pub fn specification(&self) -> String {
        format!("127.0.0.1:{}:{}", self.host, self.container)
    }
    pub fn description(&self) -> String {
        format!(
            "http://127.0.0.1:{} -> container:{} (TCP)",
            self.host, self.container
        )
    }
}
pub fn project_id(root: &Path) -> String {
    format!(
        "jail-{}",
        &digest(root.as_os_str().as_encoded_bytes())[..16]
    )
}
fn directory_slug(root: &Path) -> String {
    let directory = root.file_name().unwrap_or_default().to_string_lossy();
    let mut slug = String::new();
    for c in directory.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 28 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "project".into()
    } else {
        slug.into()
    }
}
impl Project {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&cwd)
            .output();
        let root = match output {
            Ok(o) if o.status.success() => {
                PathBuf::from(String::from_utf8(o.stdout)?.trim_end()).canonicalize()?
            }
            _ => cwd.clone(),
        };
        if !cwd.starts_with(&root) {
            bail!("working directory is outside the Git root");
        }
        validate_mount_path(&root)?;
        // Stable identity is only for locks, metadata and the persistent home.
        // The visible container name is independent and stored in Record.id.
        let id = project_id(&root);
        Ok(Self { root, cwd, id })
    }
    pub fn new_container_name(&self) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let seconds = now.as_secs() as libc::time_t;
        let mut date: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&seconds, &mut date) }.is_null() {
            bail!("cannot format container creation time");
        }
        Ok(format!(
            "jail-{}-{:04}{:02}{:02}-{:02}{:02}{:02}-{:06}",
            directory_slug(&self.root),
            date.tm_year + 1900,
            date.tm_mon + 1,
            date.tm_mday,
            date.tm_hour,
            date.tm_min,
            date.tm_sec,
            now.subsec_micros()
        ))
    }
    pub fn record_path(&self, data: &Path) -> PathBuf {
        data.join("projects").join(&self.id).join("record.json")
    }
    pub fn record(&self, data: &Path) -> Result<Option<Record>> {
        read_record(&self.record_path(data))
    }
    pub fn save(&self, data: &Path, record: &Record) -> Result<()> {
        let path = self.record_path(data);
        private_dir(path.parent().unwrap())?;
        let tmp = path.with_extension("json.new");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec_pretty(record)?)?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}
fn read_record(path: &Path) -> Result<Option<Record>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid jail state")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("state directory cannot be a symlink");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn lock(data: &Path, id: &str, name: &str) -> Result<File> {
    let dir = data.join("projects").join(id);
    private_dir(&dir)?;
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{name}.lock")))?)
}
pub fn records(data: &Path) -> Result<Vec<Record>> {
    let dir = data.join("projects");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(dir)? {
        if let Some(record) = read_record(&entry?.path().join("record.json"))? {
            out.push(record);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_readable_names_and_stable_project_keys() {
        let root = PathBuf::from("/tmp/My Project_01!");
        assert_eq!(directory_slug(&root), "my-project-01");
        assert_eq!(directory_slug(Path::new("/tmp/日本語")), "project");
        let project = Project {
            id: project_id(&root),
            cwd: root.clone(),
            root,
        };
        let name = project.new_container_name().unwrap();
        assert!(name.starts_with("jail-my-project-01-"));
        assert!(name.len() <= 63);
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        assert_eq!(project.id, project_id(&project.root));
        assert_ne!(project.id, name);
    }
    #[test]
    fn legacy_records_without_ports_are_readable() {
        let record: Record = serde_json::from_str(
            r#"{"id":"jail-old","root":"/tmp/app","spec":"s","image":"i","volume":"v"}"#,
        )
        .unwrap();
        assert!(record.ports.is_empty());
    }
}
