//! Explicit removal of jail-owned home volumes and images. Only names in jail's
//! own formats are touched, one by one; resources still used by any container
//! are refused, or skipped by --all-delete.
use crate::{
    backend::Backend,
    project::{self, Project, Record},
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use fs2::FileExt;
use serde_json::Value;
use std::{fs, path::Path};

#[derive(Subcommand)]
pub enum Target {
    /// Delete unused home volumes, including that project's CLI settings and history.
    Volume {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Delete unused jail images; a later launch rebuilds the configured one.
    Image {
        #[arg(required = true)]
        tags: Vec<String>,
    },
}

fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn volume_key(name: &str) -> Option<&str> {
    let key = name.strip_suffix("-home")?;
    hex(key.strip_prefix("jail-")?, 16).then_some(key)
}
fn image_name(name: &str) -> Option<&str> {
    let name = name.strip_prefix("docker.io/library/").unwrap_or(name);
    name.strip_prefix("jail-runtime:")
        .is_some_and(|h| hex(h, 20))
        .then_some(name)
}
fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}
fn container_id(entry: &Value) -> Option<&str> {
    entry
        .get("id")
        .or_else(|| entry.pointer("/configuration/id"))
        .and_then(Value::as_str)
}
/// Containers that may use `name`. The container JSON layout differs between
/// backend versions, so any mention counts; jail state covers its own records.
fn users(containers: &[Value], records: &[Record], name: &str) -> Vec<String> {
    let mut out = vec![];
    for entry in containers {
        let id = container_id(entry).unwrap_or("unnamed container");
        let mut text = vec![];
        strings(entry, &mut text);
        let recorded = records
            .iter()
            .any(|r| r.id == id && (r.volume == name || r.image == name));
        if recorded || text.iter().any(|s| s.contains(name)) {
            out.push(id.to_owned());
        }
    }
    out
}
fn exists(backend: &Backend, kind: &str, name: &str) -> bool {
    backend
        .command(&[kind, "inspect", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
fn listed(backend: &Backend, kind: &str) -> Result<Vec<Value>> {
    let value: Value =
        serde_json::from_slice(&backend.capture(&[kind, "list", "--format", "json"])?)?;
    value
        .as_array()
        .cloned()
        .with_context(|| format!("unexpected {kind} list JSON"))
}
fn volume_entry(entry: &Value) -> Option<&str> {
    entry
        .get("id")
        .or_else(|| entry.pointer("/configuration/name"))
        .and_then(Value::as_str)
        .filter(|name| volume_key(name).is_some())
}
fn image_entry(entry: &Value) -> Option<&str> {
    entry
        .pointer("/configuration/name")
        .and_then(Value::as_str)
        .and_then(image_name)
}
/// Serialize with image builds and container creation, which re-checks the
/// image under the allocation lock before running.
fn image_locks(data: &Path) -> Result<[fs::File; 2]> {
    let build = project::lock(data, "image", "build")?;
    FileExt::lock_exclusive(&build)?;
    let allocation = project::lock(data, "port-forwarding", "allocation")?;
    FileExt::lock_exclusive(&allocation)?;
    Ok([build, allocation])
}
fn date(entry: &Value) -> &str {
    let date = entry
        .pointer("/configuration/creationDate")
        .and_then(Value::as_str)
        .unwrap_or("");
    date.get(..10).unwrap_or(date)
}

pub fn run(
    target: Option<Target>,
    all_delete: bool,
    backend: &Backend,
    data: &Path,
    project: &Project,
    current: &str,
) -> Result<i32> {
    backend.ensure_service()?;
    match target {
        None if all_delete => delete_all(backend, data),
        None => list(backend, data, project, current),
        Some(Target::Volume { mut names }) => {
            names.sort();
            names.dedup();
            for name in &names {
                volume_key(name).with_context(|| {
                    format!("{name} is not a jail home volume (expected jail-<16 hex>-home)")
                })?;
            }
            for name in &names {
                remove_volume(backend, data, name, false)?;
            }
            Ok(0)
        }
        Some(Target::Image { mut tags }) => {
            tags.sort();
            tags.dedup();
            for tag in &tags {
                if image_name(tag) != Some(tag.as_str()) {
                    bail!("{tag} is not a jail image (expected jail-runtime:<20 hex>)");
                }
            }
            let _locks = image_locks(data)?;
            for tag in &tags {
                remove_image(backend, data, tag, false)?;
            }
            Ok(0)
        }
    }
}

/// Deletes every jail-owned home volume and image that no container uses.
/// Other volumes and images are never listed as candidates.
fn delete_all(backend: &Backend, data: &Path) -> Result<i32> {
    let volumes: Vec<String> = listed(backend, "volume")?
        .iter()
        .filter_map(volume_entry)
        .map(str::to_owned)
        .collect();
    let images: Vec<String> = listed(backend, "image")?
        .iter()
        .filter_map(image_entry)
        .map(str::to_owned)
        .collect();
    let (mut removed, mut skipped) = (0, 0);
    for name in &volumes {
        if remove_volume(backend, data, name, true)? {
            removed += 1;
        } else {
            skipped += 1;
        }
    }
    let _locks = image_locks(data)?;
    for tag in &images {
        if remove_image(backend, data, tag, true)? {
            removed += 1;
        } else {
            skipped += 1;
        }
    }
    println!("removed {removed}, skipped {skipped} in use");
    Ok(0)
}

/// Returns false when `skip_used` is set and a container still uses the volume.
fn remove_volume(backend: &Backend, data: &Path, name: &str, skip_used: bool) -> Result<bool> {
    let key = volume_key(name).context("not a jail home volume")?;
    // Launch, stop and recreate of that project wait while this runs.
    let mutation = project::lock(data, key, "lifecycle")?;
    FileExt::lock_exclusive(&mutation)?;
    let users = users(&backend.list()?, &project::records(data)?, name);
    if !users.is_empty() && skip_used {
        println!("skipped home volume {name}: in use by {}", users.join(", "));
        return Ok(false);
    }
    if !users.is_empty() {
        bail!(
            "{name} is used by container {}; close its sessions and run jail recreate in that project first",
            users.join(", ")
        );
    }
    if !exists(backend, "volume", name) {
        bail!("home volume {name} does not exist");
    }
    backend.checked(&["volume", "delete", name])?;
    println!("removed home volume {name}");
    Ok(true)
}

/// Returns false when `skip_used` is set and a container still uses the image.
/// The caller holds `image_locks`.
fn remove_image(backend: &Backend, data: &Path, tag: &str, skip_used: bool) -> Result<bool> {
    let users = users(&backend.list()?, &project::records(data)?, tag);
    if !users.is_empty() && skip_used {
        println!("skipped image {tag}: in use by {}", users.join(", "));
        return Ok(false);
    }
    if !users.is_empty() {
        bail!(
            "{tag} is used by container {}; recreate those projects first",
            users.join(", ")
        );
    }
    if !exists(backend, "image", tag) {
        bail!("image {tag} does not exist");
    }
    backend.checked(&["image", "delete", tag])?;
    // The build context holds only jail source and version pins.
    let context = data.join("build").join(tag.replace(':', "-"));
    if context.exists() {
        fs::remove_dir_all(&context)?;
    }
    println!("removed image {tag}");
    Ok(true)
}

fn list(backend: &Backend, data: &Path, project: &Project, current: &str) -> Result<i32> {
    let containers = backend.list()?;
    let records = project::records(data)?;
    let state = |name: &str| {
        let users = users(&containers, &records, name);
        if users.is_empty() {
            "unused".to_owned()
        } else {
            format!("in use by {}", users.join(", "))
        }
    };
    println!("Home volumes (deleting one removes that project's CLI settings and history):");
    let mut count = 0;
    for entry in listed(backend, "volume")? {
        let Some(name) = volume_entry(&entry) else {
            continue;
        };
        let key = volume_key(name).context("not a jail home volume")?;
        // Volumes created before labels were added identify only this project.
        let root = records
            .iter()
            .find(|r| r.volume == name)
            .map(|r| r.root.display().to_string())
            .or_else(|| {
                entry
                    .pointer("/configuration/labels/io.jail.project")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| (key == project.id).then(|| project.root.display().to_string()))
            .unwrap_or_else(|| "project unknown".into());
        println!(
            "  {name}  {}  created {}  {root}",
            state(name),
            date(&entry)
        );
        count += 1;
    }
    if count == 0 {
        println!("  none");
    }
    println!("Images:");
    count = 0;
    for entry in listed(backend, "image")? {
        let Some(tag) = image_entry(&entry) else {
            continue;
        };
        let size: u64 = entry["variants"]
            .as_array()
            .map(|v| v.iter().filter_map(|v| v["size"].as_u64()).sum())
            .unwrap_or(0);
        println!(
            "  {tag}  {}{}  created {}  {} MiB",
            state(tag),
            if tag == current {
                ", current config"
            } else {
                ""
            },
            date(&entry),
            size / 1024 / 1024
        );
        count += 1;
    }
    if count == 0 {
        println!("  none");
    }
    println!("Delete explicitly: jail cleanup volume <name>... / jail cleanup image <tag>...");
    println!("Delete every unused one: jail cleanup --all-delete");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_jail_names_are_accepted() {
        assert_eq!(
            volume_key("jail-0123456789abcdef-home"),
            Some("jail-0123456789abcdef")
        );
        for name in [
            "jail-0123456789ABCDEF-home",
            "jail-0123456789abcde-home",
            "jail-0123456789abcdef",
            "other-0123456789abcdef-home",
            "jail-../../../etc-home",
        ] {
            assert_eq!(volume_key(name), None);
        }
        let tag = "jail-runtime:0123456789abcdef0123";
        assert_eq!(image_name(tag), Some(tag));
        assert_eq!(image_name(&format!("docker.io/library/{tag}")), Some(tag));
        assert_eq!(image_name("jail-runtime:latest"), None);
        assert_eq!(image_name("docker.io/library/node:22"), None);
    }
    #[test]
    fn any_container_mention_counts_as_use() {
        let containers = vec![
            serde_json::json!({"configuration": {"id": "a", "mounts": [{"source": "/v/jail-0123456789abcdef-home/volume.img"}]}}),
            serde_json::json!({"configuration": {"id": "b"}}),
        ];
        assert_eq!(
            users(&containers, &[], "jail-0123456789abcdef-home"),
            vec!["a"]
        );
        assert!(users(&containers, &[], "jail-fedcba9876543210-home").is_empty());
    }
}
