//! Host-side preparation of mount boundaries. Never overwrite existing content.
//! Absent sensitive paths receive explicit placeholders, like srt's phantom paths.
use anyhow::{bail, Context, Result};
use jail::config::{validate_mount_path, Config};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const DEFAULTS: &[(&str, bool)] = &[
    (".git/config", false),
    (".git/hooks", true),
    (".mcp.json", false),
    (".claude", true),
    (".codex", true),
    (".bashrc", false),
    (".zshrc", false),
    (".ripgreprc", false),
];
#[derive(serde::Serialize)]
pub struct Guard {
    pub anchors: Vec<PathBuf>,
    pub readonly: Vec<PathBuf>,
}
fn safe_components(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            bail!("invalid protected path");
        }
        path.push(component);
        validate_mount_path(&path)?;
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                bail!(
                    "protected path cannot traverse a symlink: {}",
                    path.display()
                );
            }
            if meta.is_file() && meta.nlink() > 1 {
                bail!("protected file has hard-link aliases: {}", path.display());
            }
        }
    }
    Ok(path)
}
pub fn prepare(root: &Path, cfg: &Config) -> Result<Guard> {
    let mut guard = Guard {
        anchors: vec![],
        readonly: vec![],
    };
    if cfg.workspace.read_only || !cfg.tools.edit {
        return Ok(guard);
    }
    let mut rules: Vec<(String, bool)> = if cfg.workspace.protect_sensitive {
        DEFAULTS.iter().map(|(p, d)| (p.to_string(), *d)).collect()
    } else {
        vec![]
    };
    rules.extend(
        cfg.workspace
            .deny_write
            .iter()
            .map(|p| (p.clone(), p.ends_with('/'))),
    );
    // A worktree's .git is a file, not a directory. Protect that pointer; external
    // worktree metadata is never implicitly shared.
    let git_file = cfg.workspace.protect_sensitive && root.join(".git").is_file();
    if git_file {
        rules.retain(|(p, _)| !p.starts_with(".git/"));
        rules.push((".git".into(), false));
    }
    for (relative, directory) in rules {
        if cfg
            .workspace
            .hidden
            .iter()
            .any(|h| Path::new(&relative).starts_with(h))
        {
            continue;
        }
        let target = safe_components(root, &relative)?;
        // Anchor each parent as a mount point: moving a parent must not create a
        // new writable alias at the protected pathname.
        let parent = target.parent().context("protected path has no parent")?;
        fs::create_dir_all(parent)?;
        let mut ancestor = parent;
        while ancestor != root {
            if !ancestor.starts_with(root) {
                bail!("protected path escaped workspace");
            }
            guard.anchors.push(ancestor.to_path_buf());
            ancestor = ancestor.parent().context("invalid protected ancestor")?;
        }
        if !target.exists() {
            if directory {
                fs::create_dir(&target)?;
            } else {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&target)?;
                if relative == ".mcp.json" {
                    file.write_all(b"{}\n")?;
                }
            }
            eprintln!(
                "jail: created write-protection placeholder {}",
                target.display()
            );
        }
        safe_components(root, &relative)?;
        // Refuse hard links anywhere in a protected directory. Cross-mount
        // hard-link creation is denied by the kernel after the readonly bind.
        check_tree(&target)?;
        guard.readonly.push(target);
    }
    // Never create a writable submount below a protected directory.
    guard.anchors.retain(|p| {
        !guard
            .readonly
            .iter()
            .any(|r| r.is_dir() && p.starts_with(r))
    });
    guard
        .anchors
        .sort_by(|a, b| (a.components().count(), a).cmp(&(b.components().count(), b)));
    guard.anchors.dedup();
    guard.readonly.sort();
    guard.readonly.dedup();
    for mount in &cfg.workspace.mounts {
        if !mount.read_only
            && (mount.source.starts_with(root) || root.starts_with(&mount.source))
            && !guard.readonly.is_empty()
        {
            bail!("writable extra mounts overlapping the workspace can bypass write protection; use a read-only extra mount");
        }
    }
    Ok(guard)
}
fn check_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || (meta.is_file() && meta.nlink() > 1) {
        bail!(
            "protected path has a symlink or hard-link alias: {}",
            path.display()
        );
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            check_tree(&entry?.path())?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_preserve_existing_files_and_protect_absent_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/config"), "[core]\n").unwrap();
        let guard = prepare(tmp.path(), &Config::default()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert!(guard.anchors.contains(&tmp.path().join(".git")));
        assert!(guard.readonly.contains(&tmp.path().join(".mcp.json")));
        assert_eq!(
            fs::read_to_string(tmp.path().join(".mcp.json")).unwrap(),
            "{}\n"
        );
    }
    #[test]
    fn symlinks_and_hardlinks_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("alias"), "x").unwrap();
        std::os::unix::fs::symlink("alias", tmp.path().join(".mcp.json")).unwrap();
        assert!(prepare(tmp.path(), &Config::default()).is_err());
        fs::remove_file(tmp.path().join(".mcp.json")).unwrap();
        fs::hard_link(tmp.path().join("alias"), tmp.path().join(".mcp.json")).unwrap();
        assert!(prepare(tmp.path(), &Config::default()).is_err());
    }
    #[test]
    fn custom_rules_cannot_create_writable_submounts_under_protected_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.workspace.protect_sensitive = false;
        cfg.workspace.deny_write = vec!["trusted/".into(), "trusted/nested/config".into()];
        let guard = prepare(tmp.path(), &cfg).unwrap();
        assert!(guard.anchors.is_empty());
        assert!(guard.readonly.contains(&tmp.path().join("trusted")));
    }
    #[test]
    fn worktree_pointer_is_protected_and_mutable_alias_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: /outside/worktree\n").unwrap();
        let mut cfg = Config::default();
        let guard = prepare(tmp.path(), &cfg).unwrap();
        assert!(guard.readonly.contains(&tmp.path().join(".git")));
        cfg.workspace.mounts.push(jail::config::Mount {
            source: tmp.path().into(),
            target: "/extra/alias".into(),
            read_only: false,
        });
        assert!(prepare(tmp.path(), &cfg).is_err());
    }
}
