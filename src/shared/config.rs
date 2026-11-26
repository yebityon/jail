use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub resources: Resources,
    pub workspace: Workspace,
    pub network: Network,
    pub tools: Tools,
    pub auth: Auth,
    pub lifecycle: Lifecycle,
    pub image: Image,
    pub environment: Environment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    pub cpus: u32,
    pub memory: String,
}
impl Default for Resources {
    fn default() -> Self {
        Self {
            cpus: 4,
            memory: "4G".into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspace {
    pub read_only: bool,
    pub mounts: Vec<Mount>,
    pub hidden: Vec<String>,
    pub protect_sensitive: bool,
    pub deny_write: Vec<String>,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            read_only: false,
            mounts: vec![],
            hidden: vec![],
            protect_sensitive: true,
            deny_write: vec![],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    #[serde(default = "yes")]
    pub read_only: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    Open,
    None,
    Allowlist,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub mode: NetworkMode,
    pub allowed_hosts: Vec<String>,
    pub allowed_ports: Vec<u16>,
    pub allow_private_ips: bool,
    /// TCP ports published exclusively on the Mac's IPv4 loopback interface.
    pub published_ports: Vec<u16>,
    pub auto_port: bool,
}
impl Default for Network {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Open,
            allowed_hosts: vec![],
            allowed_ports: vec![443],
            allow_private_ips: false,
            published_ports: vec![3000, 5173, 8000, 8080],
            auto_port: true,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tools {
    pub shell: bool,
    pub edit: bool,
    pub web_search: bool,
    pub mcp: bool,
    pub hooks: bool,
    pub claude_deny: Vec<String>,
    pub codex_disabled_features: Vec<String>,
}
impl Default for Tools {
    fn default() -> Self {
        Self {
            shell: true,
            edit: true,
            web_search: true,
            mcp: false,
            hooks: false,
            claude_deny: vec![],
            codex_disabled_features: vec![],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    pub import_host: bool,
}
impl Default for Auth {
    fn default() -> Self {
        Self { import_host: true }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lifecycle {
    pub auto_stop: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Image {
    pub claude_version: String,
    pub codex_version: String,
}
impl Default for Image {
    fn default() -> Self {
        Self {
            claude_version: "2.1.284".into(),
            codex_version: "0.159.2".into(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Environment {
    pub pass: Vec<String>,
    pub set: BTreeMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg: Self = if path.exists() {
            toml::from_str(&fs::read_to_string(path)?)
                .with_context(|| format!("invalid config: {}", path.display()))?
        } else {
            Self::default()
        };
        for mount in &mut cfg.workspace.mounts {
            if !mount.source.is_absolute() {
                mount.source = path.parent().unwrap_or(Path::new(".")).join(&mount.source);
            }
            mount.source = mount
                .source
                .canonicalize()
                .context("mount source does not exist")?;
        }
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        for feature in &self.tools.codex_disabled_features {
            if feature.is_empty()
                || !feature
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                bail!("invalid Codex feature name: {feature}");
            }
        }
        if self
            .tools
            .claude_deny
            .iter()
            .any(|rule| rule.is_empty() || rule.contains(['\n', '\r']))
        {
            bail!("Claude tool denial rules must be non-empty single lines");
        }
        if self.resources.cpus == 0 {
            bail!("resources.cpus must be positive");
        }
        let memory = &self.resources.memory;
        let number = memory.trim_end_matches(['K', 'M', 'G', 'T']);
        if number.is_empty()
            || number.parse::<u64>().unwrap_or(0) == 0
            || memory.len() > number.len() + 1
        {
            bail!("resources.memory must be a positive amount such as 4G");
        }
        for version in [&self.image.claude_version, &self.image.codex_version] {
            if version.is_empty()
                || !version
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
            {
                bail!("image versions must be exact versions or release channels");
            }
        }
        if self.network.mode == NetworkMode::Allowlist && self.network.allowed_hosts.is_empty() {
            bail!("network.allowed_hosts must not be empty in allowlist mode");
        }
        if self.network.allowed_ports.is_empty() || self.network.allowed_ports.contains(&0) {
            bail!("network.allowed_ports must contain valid ports");
        }
        let unique: std::collections::BTreeSet<_> = self.network.published_ports.iter().collect();
        if self.network.published_ports.contains(&0)
            || unique.len() != self.network.published_ports.len()
            || self.network.published_ports.len() > 64
        {
            bail!("network.published_ports must contain at most 64 unique non-zero TCP ports");
        }
        for host in &self.network.allowed_hosts {
            let name = host.strip_prefix("*.").unwrap_or(host);
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
                || name.starts_with('.')
                || name.ends_with('.')
                || name.contains("..")
            {
                bail!("invalid allowed host: {host}; use an exact hostname or *.example.com");
            }
        }
        let mut targets = std::collections::BTreeSet::new();
        for mount in &self.workspace.mounts {
            let target = mount
                .target
                .to_str()
                .context("mount target must be UTF-8")?;
            if !target.starts_with("/extra/") || target.contains("..") || !targets.insert(target) {
                bail!("mount targets must be unique paths under /extra/");
            }
            validate_mount_path(&mount.source)?;
            validate_mount_path(&mount.target)?;
        }
        for hidden in self
            .workspace
            .hidden
            .iter()
            .chain(&self.workspace.deny_write)
        {
            let path = Path::new(hidden);
            if hidden.is_empty()
                || path.is_absolute()
                || path
                    .components()
                    .any(|p| !matches!(p, std::path::Component::Normal(_)))
            {
                bail!("workspace.hidden/deny_write entries must be relative paths without ..");
            }
        }
        for key in self
            .environment
            .pass
            .iter()
            .chain(self.environment.set.keys())
        {
            if key.is_empty()
                || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || key.as_bytes()[0].is_ascii_digit()
            {
                bail!("invalid environment variable name: {key}");
            }
            if reserved_env(key) {
                bail!("environment variable {key} is reserved by jail");
            }
        }
        Ok(())
    }
}
pub fn reserved_env(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "NODE_OPTIONS",
        "BUN_OPTIONS",
        "PYTHONPATH",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
    ]
    .contains(&key.as_str())
        || key.starts_with("JAIL_")
        || key.ends_with("_PROXY")
        || key == "NO_PROXY"
}
pub fn validate_mount_path(path: &Path) -> Result<()> {
    let s = path.to_str().context("mount paths must be UTF-8")?;
    if s.contains([',', '\n', '\r']) {
        bail!(
            "mount path cannot contain comma or newline: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_settings_and_unsafe_mounts_fail() {
        assert!(toml::from_str::<Config>("[network]\nmod = 'none'").is_err());
        let mut c = Config::default();
        c.workspace.mounts.push(Mount {
            source: "/tmp".into(),
            target: "/etc".into(),
            read_only: false,
        });
        assert!(c.validate().is_err());
        c.workspace.mounts.clear();
        c.environment.pass.push("https_proxy".into());
        assert!(c.validate().is_err());
    }
    #[test]
    fn allowlist_and_hidden_paths_validate() {
        let mut c = Config::default();
        c.network.mode = NetworkMode::Allowlist;
        assert!(c.validate().is_err());
        c.network.allowed_hosts = vec!["*.anthropic.com".into()];
        assert!(c.validate().is_ok());
        c.workspace.hidden.push("../secret".into());
        assert!(c.validate().is_err());
    }
    #[test]
    fn port_defaults_and_validation() {
        let mut c: Config = toml::from_str("[network]\nmode='open'").unwrap();
        assert_eq!(c.network.published_ports, [3000, 5173, 8000, 8080]);
        assert!(c.network.auto_port);
        c.network.published_ports = vec![];
        assert!(c.validate().is_ok());
        for ports in [vec![0], vec![3000, 3000], (1..=65).collect()] {
            c.network.published_ports = ports;
            assert!(c.validate().is_err());
        }
    }
}
