use super::Client;
use anyhow::Result;
use serde_json::Value;
use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::Command,
};

/// One container as `list` or `inspect` prints it. The JSON layout differs
/// between versions, so each field is read from every place it can be.
#[derive(Debug, Clone)]
pub struct Container(Value);
impl From<Value> for Container {
    fn from(value: Value) -> Self {
        Self(value)
    }
}
impl Container {
    /// `inspect` wraps the container in an array.
    fn inspected(value: Value) -> Self {
        match value {
            Value::Array(mut entries) if !entries.is_empty() => Self(entries.swap_remove(0)),
            other => Self(other),
        }
    }
    /// The container name.
    pub fn id(&self) -> Option<&str> {
        self.0
            .get("id")
            .or_else(|| self.0.pointer("/configuration/id"))
            .and_then(Value::as_str)
    }
    /// Lowercase state, such as `running` or `stopped`.
    pub fn state(&self) -> Option<String> {
        self.0
            .pointer("/status/state")
            .or_else(|| self.0.get("status"))
            .or_else(|| self.0.get("state"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    }
    pub fn label(&self, key: &str) -> Option<&str> {
        [
            self.0.pointer("/configuration/labels"),
            self.0.get("labels"),
        ]
        .into_iter()
        .flatten()
        .find_map(|labels| labels.get(key))
        .and_then(Value::as_str)
    }
    /// Whether any string in the JSON contains `text`, wherever this version
    /// stores mounts, images and other references.
    pub fn mentions(&self, text: &str) -> bool {
        fn mentions(value: &Value, text: &str) -> bool {
            match value {
                Value::String(s) => s.contains(text),
                Value::Array(items) => items.iter().any(|v| mentions(v, text)),
                Value::Object(map) => map.values().any(|v| mentions(v, text)),
                _ => false,
            }
        }
        mentions(&self.0, text)
    }
}

/// Options of `container run`, passed in the order they are added.
pub struct Run {
    name: String,
    image: String,
    detach: bool,
    options: Vec<String>,
}
impl Run {
    pub fn new(name: &str, image: &str) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            detach: false,
            options: vec![],
        }
    }
    fn option(&mut self, flag: &str, value: String) -> &mut Self {
        self.options.extend([flag.into(), value]);
        self
    }
    pub fn detach(&mut self) -> &mut Self {
        self.detach = true;
        self
    }
    pub fn label(&mut self, key: &str, value: &str) -> &mut Self {
        self.option("--label", format!("{key}={value}"))
    }
    pub fn cpus(&mut self, cpus: u32) -> &mut Self {
        self.option("--cpus", cpus.to_string())
    }
    /// `memory` uses the command's units, such as `4G`.
    pub fn memory(&mut self, memory: &str) -> &mut Self {
        self.option("--memory", memory.into())
    }
    pub fn cap_drop(&mut self, capability: &str) -> &mut Self {
        self.option("--cap-drop", capability.into())
    }
    pub fn cap_add(&mut self, capability: &str) -> &mut Self {
        self.option("--cap-add", capability.into())
    }
    pub fn tmpfs(&mut self, target: &str) -> &mut Self {
        self.option("--tmpfs", target.into())
    }
    pub fn bind(&mut self, source: &Path, target: &Path, read_only: bool) -> &mut Self {
        self.option(
            "--mount",
            format!(
                "type=bind,source={},target={}{}",
                source.display(),
                target.display(),
                if read_only { ",readonly" } else { "" }
            ),
        )
    }
    pub fn volume(&mut self, name: &str, target: &str) -> &mut Self {
        self.option(
            "--mount",
            format!("type=volume,source={name},target={target}"),
        )
    }
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.option("--env", format!("{key}={value}"))
    }
    pub fn masked_path(&mut self, path: &Path) -> &mut Self {
        self.option("--masked-path", path.to_string_lossy().into_owned())
    }
    /// `spec` is `[host-ip:]host-port:container-port`.
    pub fn publish(&mut self, spec: &str) -> &mut Self {
        self.option("--publish", spec.into())
    }
    fn args(&self) -> Vec<String> {
        let mut args = vec!["run".to_owned()];
        if self.detach {
            args.push("--detach".into());
        }
        args.extend(["--name".into(), self.name.clone()]);
        args.extend(self.options.iter().cloned());
        args.push(self.image.clone());
        args
    }
}

/// Options of `container exec`. They are passed in a fixed order, whatever
/// order they are set in.
pub struct Exec<'a> {
    client: &'a Client,
    container: String,
    interactive: bool,
    env: Vec<String>,
    tty: bool,
    user: Option<String>,
    workdir: Option<String>,
}
impl Exec<'_> {
    /// Keep stdin open.
    pub fn interactive(&mut self) -> &mut Self {
        self.interactive = true;
        self
    }
    /// Pass `key` with its value in this process's environment.
    pub fn pass_env(&mut self, key: &str) -> &mut Self {
        self.env.push(key.into());
        self
    }
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.env.push(format!("{key}={value}"));
        self
    }
    pub fn tty(&mut self) -> &mut Self {
        self.tty = true;
        self
    }
    pub fn user(&mut self, user: &str) -> &mut Self {
        self.user = Some(user.into());
        self
    }
    pub fn workdir(&mut self, dir: &Path) -> &mut Self {
        self.workdir = Some(dir.to_string_lossy().into_owned());
        self
    }
    /// A command running `argv` in the container. The caller sets up its
    /// stdio and runs it.
    pub fn command<S: AsRef<OsStr>>(&self, argv: &[S]) -> Command {
        let mut args: Vec<OsString> = vec!["exec".into()];
        if self.interactive {
            args.push("--interactive".into());
        }
        for env in &self.env {
            args.extend(["--env".into(), env.into()]);
        }
        if self.tty {
            args.push("--tty".into());
        }
        if let Some(user) = &self.user {
            args.extend(["--user".into(), user.into()]);
        }
        if let Some(dir) = &self.workdir {
            args.extend(["--workdir".into(), dir.into()]);
        }
        args.push(self.container.as_str().into());
        args.extend(argv.iter().map(|a| a.as_ref().to_owned()));
        self.client.command(&args)
    }
}

impl Client {
    /// Every container, running or not.
    pub fn list(&self) -> Result<Vec<Container>> {
        let entries = self.entries(&["list", "--all", "--format", "json"], "container")?;
        Ok(entries.into_iter().map(Container).collect())
    }
    pub fn inspect(&self, name: &str) -> Result<Container> {
        let value: Value = serde_json::from_slice(&self.capture(&["inspect", name])?)?;
        Ok(Container::inspected(value))
    }
    pub fn run(&self, run: &Run) -> Result<()> {
        self.checked(&run.args())
    }
    pub fn start(&self, name: &str) -> Result<()> {
        self.checked(&["start", name])
    }
    pub fn stop(&self, name: &str) -> Result<()> {
        self.checked(&["stop", name])
    }
    pub fn delete(&self, name: &str) -> Result<()> {
        self.checked(&["delete", name])
    }
    pub fn exec(&self, container: &str) -> Exec<'_> {
        Exec {
            client: self,
            container: container.into(),
            interactive: false,
            env: vec![],
            tty: false,
            user: None,
            workdir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn both_container_json_layouts_are_read() {
        let flat = Container::from(
            json!({"id": "a", "labels": {"io.jail.project": "/work/app"}, "status": "Running"}),
        );
        let nested = json!({"configuration": {"id": "b", "labels": {"io.jail.project": "/work/app"}},
            "status": {"state": "stopped"}});
        assert_eq!(flat.id(), Some("a"));
        assert_eq!(flat.label("io.jail.project"), Some("/work/app"));
        assert_eq!(flat.state().as_deref(), Some("running"));
        let inspected = Container::inspected(json!([nested.clone()]));
        for entry in [Container::from(nested), inspected] {
            assert_eq!(entry.id(), Some("b"));
            assert_eq!(entry.label("io.jail.project"), Some("/work/app"));
            assert_eq!(entry.state().as_deref(), Some("stopped"));
        }
        assert_eq!(Container::inspected(json!([])).id(), None);
        assert_eq!(flat.label("io.jail.other"), None);
        assert_eq!(
            Container::from(json!({"state": "Exited"}))
                .state()
                .as_deref(),
            Some("exited")
        );
        assert_eq!(Container::from(json!({"id": "c"})).state(), None);
    }
    #[test]
    fn mentions_searches_every_string() {
        let entry = Container::from(
            json!({"configuration": {"mounts": [{"source": "/v/jail-0123456789abcdef-home/volume.img"}]}}),
        );
        assert!(entry.mentions("jail-0123456789abcdef-home"));
        assert!(!entry.mentions("jail-fedcba9876543210-home"));
    }
    #[test]
    fn run_and_exec_options_keep_the_command_order() {
        let mut run = Run::new("box", "image:tag");
        run.label("k", "v")
            .detach()
            .bind(Path::new("/w"), Path::new("/w"), true)
            .volume("home", "/home/jail")
            .env("A", "1")
            .publish("127.0.0.1:1:2");
        assert_eq!(
            run.args(),
            [
                "run",
                "--detach",
                "--name",
                "box",
                "--label",
                "k=v",
                "--mount",
                "type=bind,source=/w,target=/w,readonly",
                "--mount",
                "type=volume,source=home,target=/home/jail",
                "--env",
                "A=1",
                "--publish",
                "127.0.0.1:1:2",
                "image:tag",
            ]
        );
        let client = Client::new("container");
        let command = client
            .exec("box")
            .user("root")
            .tty()
            .pass_env("KEY")
            .interactive()
            .env("TERM", "xterm")
            .workdir(Path::new("/w"))
            .command(&["sh", "-c", "true"]);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "exec",
                "--interactive",
                "--env",
                "KEY",
                "--env",
                "TERM=xterm",
                "--tty",
                "--user",
                "root",
                "--workdir",
                "/w",
                "box",
                "sh",
                "-c",
                "true",
            ]
        );
    }
}
