use super::Client;
use anyhow::Result;
use serde_json::Value;

/// One volume as `volume list` prints it.
#[derive(Debug, Clone)]
pub struct Volume(Value);
impl Volume {
    pub fn name(&self) -> Option<&str> {
        self.0
            .get("id")
            .or_else(|| self.0.pointer("/configuration/name"))
            .and_then(Value::as_str)
    }
    pub fn label(&self, key: &str) -> Option<&str> {
        self.0
            .pointer("/configuration/labels")
            .and_then(|labels| labels.get(key))
            .and_then(Value::as_str)
    }
    /// Creation time as the command prints it, starting with the date.
    pub fn created(&self) -> Option<&str> {
        self.0
            .pointer("/configuration/creationDate")
            .and_then(Value::as_str)
    }
}

impl Client {
    pub fn volume_exists(&self, name: &str) -> bool {
        self.succeeds(&["volume", "inspect", name])
    }
    pub fn create_volume(&self, name: &str, labels: &[(&str, &str)]) -> Result<()> {
        let mut args = vec!["volume".to_owned(), "create".into()];
        for (key, value) in labels {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        args.push(name.into());
        self.checked(&args)
    }
    pub fn volumes(&self) -> Result<Vec<Volume>> {
        let entries = self.entries(&["volume", "list", "--format", "json"], "volume")?;
        Ok(entries.into_iter().map(Volume).collect())
    }
    pub fn delete_volume(&self, name: &str) -> Result<()> {
        self.checked(&["volume", "delete", name])
    }
}
