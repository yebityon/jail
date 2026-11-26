//! How jail uses Apple container: which executable it runs, and which
//! containers belong to which project.
use crate::{
    apple_container::{Client, Container},
    project::Record,
};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Label holding the project root on jail's containers and home volumes.
pub const PROJECT_LABEL: &str = "io.jail.project";

/// `container`, or the executable `JAIL_CONTAINER_BIN` names for testing.
pub fn client() -> Client {
    Client::new(std::env::var_os("JAIL_CONTAINER_BIN").unwrap_or("container".into()))
}
pub fn ensure_service(client: &Client) -> Result<()> {
    if client.service_running() {
        return Ok(());
    }
    eprintln!("jail: starting Apple container service");
    client.start_service()
}
/// Whether the ownership label names `root`.
fn labeled_for(entry: &Container, root: &Path) -> bool {
    entry.label(PROJECT_LABEL) == Some(root.to_string_lossy().as_ref())
}
/// State of the record's container, or None if it does not exist. Fails if
/// the container is labeled for another project.
pub fn owned_state(client: &Client, record: &Record) -> Result<Option<String>> {
    for entry in client.list()? {
        if entry.id() != Some(&record.container) {
            continue;
        }
        if !labeled_for(&entry, &record.root) {
            // Some list versions omit labels, so inspect the exact container.
            if !labeled_for(&client.inspect(&record.container)?, &record.root) {
                bail!(
                    "refusing to use container {}: ownership label mismatch",
                    record.container
                );
            }
        }
        return entry
            .state()
            .map(Some)
            .context("container state is missing");
    }
    Ok(None)
}
pub fn require_owned(client: &Client, record: &Record) -> Result<()> {
    if owned_state(client, record)?.is_none() {
        bail!("container {} does not exist", record.container);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn ownership_label_must_name_the_project_root() {
        let root = Path::new("/work/app");
        let flat = Container::from(json!({"id": "a", "labels": {"io.jail.project": "/work/app"}}));
        let nested = Container::from(
            json!({"configuration": {"id": "b", "labels": {"io.jail.project": "/work/app"}}}),
        );
        assert!(labeled_for(&flat, root) && labeled_for(&nested, root));
        assert!(!labeled_for(&flat, Path::new("/work/other")));
        assert!(!labeled_for(&Container::from(json!([])), root));
    }
}
