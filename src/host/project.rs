//! jail's data directory: each project's record and locks in
//! `projects/<key>/`, locks shared by all projects in `locks/`, and image
//! build contexts in `build/`.
use anyhow::{bail, Context, Result};
use jail::{config::validate_mount_path, digest, is_digest};
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
    pub key: String,
    /// A throwaway project with no host directory, deleted when its session ends.
    pub ephemeral: bool,
}
/// jail's record of a project's container, saved as `record.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Container name, which the backend reports as the container ID.
    #[serde(rename = "id")]
    pub container: String,
    pub root: PathBuf,
    pub spec: String,
    pub image: String,
    /// Home volume; an ephemeral container has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
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
/// Hex digits of a project key.
const KEY_DIGITS: usize = 16;
/// Stable key of the project at `root`. It names the project's locks, record
/// and home volume, never the container.
pub fn key(root: &Path) -> String {
    format!(
        "jail-{}",
        &digest(root.as_os_str().as_encoded_bytes())[..KEY_DIGITS]
    )
}
pub fn home_volume(key: &str) -> String {
    format!("{key}-home")
}
/// Project key of a home volume name, if the name has jail's format.
pub fn volume_key(name: &str) -> Option<&str> {
    let key = name.strip_suffix("-home")?;
    is_digest(key.strip_prefix("jail-")?, KEY_DIGITS).then_some(key)
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
/// Parent of ephemeral projects' roots, which never exist on the host.
const EPHEMERAL_ROOT: &str = "/ephemeral";
/// `jail-<slug>-` followed by the local creation time, down to microseconds.
fn timestamped(slug: &str) -> Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let seconds = libc::time_t::try_from(now.as_secs())?;
    let Some(date) = crate::sys::local_time(seconds) else {
        bail!("cannot format container creation time");
    };
    Ok(format!(
        "jail-{slug}-{:04}{:02}{:02}-{:02}{:02}{:02}-{:06}",
        date.tm_year + 1900,
        date.tm_mon + 1,
        date.tm_mday,
        date.tm_hour,
        date.tm_min,
        date.tm_sec,
        now.subsec_micros()
    ))
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
        // The visible container name is independent of the key and stored in
        // Record.container.
        let key = key(&root);
        Ok(Self {
            root,
            cwd,
            key,
            ephemeral: false,
        })
    }
    /// A project that shares no host directory. Its root only names the
    /// container's record and locks; nothing is mounted from it.
    pub fn ephemeral() -> Result<Self> {
        let root = Path::new(EPHEMERAL_ROOT).join(timestamped("ephemeral")?);
        Ok(Self {
            key: key(&root),
            cwd: "/home/jail".into(),
            root,
            ephemeral: true,
        })
    }
    pub fn new_container_name(&self) -> Result<String> {
        if self.ephemeral {
            // The name was chosen with the root, which must stay unique.
            let name = self
                .root
                .file_name()
                .context("ephemeral root has no name")?;
            return Ok(name.to_string_lossy().into_owned());
        }
        timestamped(&directory_slug(&self.root))
    }
    pub fn record_path(&self, data: &Path) -> PathBuf {
        record_path(data, &self.key)
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
            serde_json::from_slice(&bytes).context("invalid jail record")?,
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
fn projects(data: &Path) -> PathBuf {
    data.join("projects")
}
/// Directory of one project's record, locks and login fingerprints.
pub fn dir(data: &Path, key: &str) -> PathBuf {
    projects(data).join(key)
}
pub fn record_path(data: &Path, key: &str) -> PathBuf {
    dir(data, key).join("record.json")
}
/// Opens one project's lock file; callers lock it shared or exclusively.
/// - `lifecycle`: held exclusively while launch, stop, recreate, a policy
///   change, cleanup --all-delete or home volume deletion changes that
///   project's container.
/// - `sessions`: each running session holds it shared; stop, recreate,
///   auto-stop and cleanup --all-delete take it exclusively to prove that no
///   session is active.
/// - `auth-<tool>`: held by the one session that follows host login
///   refreshes; others only try it, never wait.
///
/// Take lifecycle before sessions, and both before any global lock.
pub fn lock(data: &Path, key: &str, name: &str) -> Result<File> {
    open_lock(&dir(data, key), name)
}
/// Opens a lock file shared by all projects.
/// - `image-build`: held while an image is built or deleted.
/// - `port-allocation`: held while host ports are chosen and a container is
///   created or restarted. Image deletion holds it too, so creation can
///   re-check that its image still exists.
///
/// Take image-build before port-allocation.
pub fn global_lock(data: &Path, name: &str) -> Result<File> {
    open_lock(&data.join("locks"), name)
}
fn open_lock(dir: &Path, name: &str) -> Result<File> {
    private_dir(dir)?;
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
    let dir = projects(data);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(dir)? {
        if let Some(record) = read_record(&entry?.path().join("record.json"))? {
            out.push(record);
        }
    }
    out.sort_by(|a, b| a.container.cmp(&b.container));
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
            key: key(&root),
            cwd: root.clone(),
            root,
            ephemeral: false,
        };
        let name = project.new_container_name().unwrap();
        assert!(name.starts_with("jail-my-project-01-"));
        assert!(name.len() <= 63);
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        assert_eq!(project.key, key(&project.root));
        assert_ne!(project.key, name);
    }
    #[test]
    fn legacy_records_without_ports_are_readable() {
        let record: Record = serde_json::from_str(
            r#"{"id":"jail-old","root":"/tmp/app","spec":"s","image":"i","volume":"v"}"#,
        )
        .unwrap();
        assert!(record.ports.is_empty());
        assert_eq!(record.volume.as_deref(), Some("v"));
    }
    #[test]
    fn ephemeral_projects_are_unique_and_share_no_host_path() {
        let a = Project::ephemeral().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = Project::ephemeral().unwrap();
        assert_ne!(a.key, b.key);
        let name = a.new_container_name().unwrap();
        assert!(name.starts_with("jail-ephemeral-") && name.len() <= 63);
        assert_eq!(a.root, Path::new(EPHEMERAL_ROOT).join(&name));
        assert_eq!(a.cwd, Path::new("/home/jail"));
    }
}
