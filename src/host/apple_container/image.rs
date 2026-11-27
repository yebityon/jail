use super::Client;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;

/// One image as `image list` prints it.
#[derive(Debug, Clone)]
pub struct Image(Value);
impl Image {
    /// The reference, possibly with a registry prefix.
    pub fn name(&self) -> Option<&str> {
        self.0
            .pointer("/configuration/name")
            .and_then(Value::as_str)
    }
    /// Bytes, summed over the image's variants.
    pub fn size(&self) -> u64 {
        self.0["variants"]
            .as_array()
            .map(|v| v.iter().filter_map(|v| v["size"].as_u64()).sum())
            .unwrap_or(0)
    }
    /// Creation time as the command prints it, starting with the date.
    pub fn created(&self) -> Option<&str> {
        self.0
            .pointer("/configuration/creationDate")
            .and_then(Value::as_str)
    }
}

impl Client {
    pub fn image_exists(&self, tag: &str) -> bool {
        self.succeeds(&["image", "inspect", tag])
    }
    pub fn images(&self) -> Result<Vec<Image>> {
        let entries = self.entries(&["image", "list", "--format", "json"], "image")?;
        Ok(entries.into_iter().map(Image).collect())
    }
    pub fn delete_image(&self, tag: &str) -> Result<()> {
        self.checked(&["image", "delete", tag])
    }
    /// Builds `context` as `tag`, showing the build output.
    pub fn build(&self, tag: &str, build_args: &[(&str, String)], context: &Path) -> Result<()> {
        let mut args = vec!["build".to_owned(), "--tag".into(), tag.into()];
        for (key, value) in build_args {
            args.extend(["--build-arg".into(), format!("{key}={value}")]);
        }
        args.push(context.to_string_lossy().into_owned());
        let status = self
            .command(&args)
            .status()
            .context("could not run Apple container")?;
        if !status.success() {
            bail!("image build failed");
        }
        Ok(())
    }
}
