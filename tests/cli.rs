use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    config: PathBuf,
    backend: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project with spaces");
        fs::create_dir(&workspace).unwrap();
        let backend = tmp.path().join("container");
        fs::write(&backend, include_bytes!("fixtures/container.py")).unwrap();
        fs::set_permissions(&backend, fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("config.toml");
        fs::write(&config, "[auth]\nimport_host = false\n").unwrap();
        Self {
            tmp,
            config,
            backend,
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_jail"));
        c.current_dir(self.tmp.path().join("project with spaces"))
            .env("JAIL_HOME", self.tmp.path().join("state"))
            .env("JAIL_CONTAINER_BIN", &self.backend)
            .env("JAIL_TEST_BACKEND", self.tmp.path())
            .args(["--config", self.config.to_str().unwrap()])
            .args(args);
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.tmp.path().join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    /// The saved record of the fixture's only project.
    fn record(&self) -> Value {
        let projects = self.tmp.path().join("state/projects");
        let project = fs::read_dir(projects).unwrap().next().unwrap().unwrap();
        serde_json::from_slice(&fs::read(project.path().join("record.json")).unwrap()).unwrap()
    }
    /// The fake backend's state of one container.
    fn container(&self, id: &str) -> Value {
        let state: Value =
            serde_json::from_slice(&fs::read(self.tmp.path().join("containers.json")).unwrap())
                .unwrap();
        state
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["configuration"]["id"] == id)
            .unwrap()
            .clone()
    }
}
fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sensitive_mounts_and_live_policy_are_enforced_in_launch_plan() {
    let f = Fixture::new();
    fs::write(&f.config, "# keep my comment\n[auth]\nimport_host=false\n[network]\nmode='allowlist'\nallowed_hosts=['example.com']\n").unwrap();
    succeeds(&f.run(&["codex", "--version"]));
    let calls = f.calls();
    let run = calls.iter().find(|a| a[0] == "run").unwrap();
    let guard: Value = serde_json::from_str(
        run.iter()
            .find_map(|a| a.strip_prefix("JAIL_WRITE_GUARD="))
            .unwrap(),
    )
    .unwrap();
    assert!(guard["readonly"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a.as_str().unwrap().ends_with("/.git/config")));
    assert!(guard["readonly"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a.as_str().unwrap().ends_with("/.claude")));
    succeeds(&f.run(&["policy", "allow", "example.org"]));
    assert!(fs::read_to_string(&f.config)
        .unwrap()
        .contains("# keep my comment"));
    let live: Value =
        serde_json::from_slice(&fs::read(f.tmp.path().join("live-policy.json")).unwrap()).unwrap();
    assert!(live["allowed_hosts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "example.org"));
    succeeds(&f.run(&["codex", "--version"]));
    let log = f.run(&["policy", "log", "--json"]);
    succeeds(&log);
    let entries: Value = serde_json::from_slice(&log.stdout).unwrap();
    assert_eq!(entries[0]["host"], "example.org");
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 1);
}

#[test]
fn api_keys_and_container_login_are_rejected() {
    let f = Fixture::new();
    for args in [
        vec!["codex", "login", "--device-auth"],
        vec!["claude", "auth", "login"],
        vec!["claude", "setup-token"],
        // Signing in from jail is not offered; sign in on the Mac instead.
        vec!["auth", "claude", "--login"],
        vec!["auth", "codex", "--login"],
    ] {
        assert!(!f.run(&args).status.success(), "{args:?}");
    }
    fs::write(&f.config, "[environment]\npass=['OPENAI_API_KEY']\n").unwrap();
    assert!(!f.run(&["codex", "--version"]).status.success());
    assert!(f.calls().is_empty());
}

#[test]
fn refreshed_host_login_reaches_running_container_and_host_is_read_only() {
    for tool in ["claude", "codex"] {
        let f = Fixture::new();
        fs::write(
            &f.config,
            "[auth]\nimport_host=true\n[network]\npublished_ports=[]\n",
        )
        .unwrap();
        // File-based host logins only; the test must never reach the Keychain.
        let home = f.tmp.path().join("home");
        let file = if tool == "claude" {
            home.join(".claude/.credentials.json")
        } else {
            home.join(".codex/auth.json")
        };
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let login = |token: &str| {
            let value = if tool == "claude" {
                serde_json::json!({"claudeAiOauth": {"accessToken": token}, "mcpServers": {}})
            } else {
                serde_json::json!({"auth_mode": "chatgpt", "tokens": {"access_token": token},
                    "OPENAI_API_KEY": "dummy-key-must-not-be-imported"})
            };
            fs::write(&file, value.to_string()).unwrap();
            value.to_string()
        };
        let launch = |guest_has_auth: bool| {
            let mut command = f.command(&[tool]);
            command
                .env("HOME", &home)
                .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
                .env("CODEX_HOME", home.join(".codex"));
            if guest_has_auth {
                command.env("JAIL_TEST_HAS_AUTH", "1");
            }
            succeeds(&command.output().unwrap());
        };
        let syncs = || {
            f.calls()
                .iter()
                .filter(|a| a.iter().any(|s| s == "sync-auth"))
                .count()
        };
        let token = |name: &str| {
            let value: Value =
                serde_json::from_slice(&fs::read(f.tmp.path().join(name)).unwrap()).unwrap();
            assert!(value.get("OPENAI_API_KEY").is_none());
            assert!(value.get("mcpServers").is_none());
            if tool == "claude" {
                value["claudeAiOauth"]["accessToken"].clone()
            } else {
                value["tokens"]["access_token"].clone()
            }
        };
        login("dummy-first");
        launch(false);
        assert_eq!(token("auth-import.json"), "dummy-first");
        // The launch import is already current; the watcher must not resend it.
        assert_eq!(syncs(), 0);
        // The host CLI refreshed while the container kept the older login.
        let host = login("dummy-refreshed-by-host");
        launch(true);
        assert_eq!(syncs(), 1);
        assert_eq!(token("auth-sync.json"), "dummy-refreshed-by-host");
        launch(true);
        assert_eq!(syncs(), 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), host);
    }
}

#[test]
fn cleanup_removes_only_named_unused_jail_volumes_and_images() {
    let f = Fixture::new();
    let plan: Value =
        serde_json::from_slice(&f.run(&["--dry-run", "codex", "--version"]).stdout).unwrap();
    let used_volume = plan["home_volume"].as_str().unwrap().to_owned();
    let used_image = plan["image"].as_str().unwrap().to_owned();
    let old_volume = "jail-0123456789abcdef-home";
    let old_image = "jail-runtime:0123456789abcdef0123";
    let items = |kind: &str| -> Value {
        serde_json::from_slice(&fs::read(f.tmp.path().join(format!("{kind}s.json"))).unwrap())
            .unwrap()
    };
    fs::write(
        f.tmp.path().join("volumes.json"),
        serde_json::json!({ old_volume: {} }).to_string(),
    )
    .unwrap();
    succeeds(&f.run(&["codex", "--version"]));
    // A new home volume records its project for later listing.
    let project = f
        .tmp
        .path()
        .join("project with spaces")
        .canonicalize()
        .unwrap();
    assert_eq!(
        items("volume")[&used_volume]["io.jail.project"],
        project.to_str().unwrap()
    );
    fs::write(
        f.tmp.path().join("images.json"),
        serde_json::json!({ &used_image: {}, old_image: {}, "docker.io/library/node:22": {} })
            .to_string(),
    )
    .unwrap();
    let out = f.run(&["cleanup"]);
    succeeds(&out);
    let listing = String::from_utf8(out.stdout).unwrap();
    let line = |name: &str| {
        listing
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} missing from:\n{listing}"))
            .to_owned()
    };
    assert!(line(&used_volume).contains("in use by jail-project-with-spaces-"));
    assert!(line(&used_volume).contains(project.to_str().unwrap()));
    assert!(line(old_volume).contains("unused"));
    assert!(line(&used_image).contains("in use by"));
    assert!(line(&used_image).contains("current config"));
    assert!(line(old_image).contains("unused"));
    assert!(!listing.contains("node:22"));

    for args in [
        vec!["cleanup", "volume", &used_volume],
        vec!["cleanup", "image", &used_image],
        vec!["cleanup", "volume", "my-data"],
        vec!["cleanup", "image", "docker.io/library/node:22"],
        vec!["cleanup", "volume"],
        vec!["--dry-run", "cleanup", "volume", old_volume],
    ] {
        assert!(!f.run(&args).status.success(), "{args:?}");
    }
    assert!(!f
        .calls()
        .iter()
        .any(|a| a.get(1).is_some_and(|s| s == "delete")));

    let context = f
        .tmp
        .path()
        .join("state/build")
        .join(old_image.replace(':', "-"));
    fs::create_dir_all(&context).unwrap();
    succeeds(&f.run(&["cleanup", "volume", old_volume]));
    succeeds(&f.run(&["cleanup", "image", old_image]));
    assert!(items("volume").get(old_volume).is_none());
    assert!(items("image").get(old_image).is_none());
    assert!(!context.exists());
    assert!(items("image").get("docker.io/library/node:22").is_some());

    // After recreate the preserved home volume is no longer used.
    succeeds(&f.run(&["recreate"]));
    succeeds(&f.run(&["cleanup", "volume", &used_volume]));
    assert!(items("volume").get(&used_volume).is_none());
    // Deletion is always by explicit name, never a bulk backend operation.
    for call in f.calls() {
        if ["volume", "image"].contains(&call[0].as_str()) {
            assert_ne!(call[1], "prune");
            if call[1] == "delete" {
                assert_eq!(call.len(), 3, "{call:?}");
            }
        }
    }
}

#[test]
fn all_delete_removes_every_jail_container_volume_and_image_and_nothing_else() {
    let f = Fixture::new();
    let plan: Value =
        serde_json::from_slice(&f.run(&["--dry-run", "codex", "--version"]).stdout).unwrap();
    let used_image = plan["image"].as_str().unwrap().to_owned();
    let items = |kind: &str| -> Value {
        serde_json::from_slice(&fs::read(f.tmp.path().join(format!("{kind}s.json"))).unwrap())
            .unwrap()
    };
    fs::write(
        f.tmp.path().join("volumes.json"),
        serde_json::json!({
            "jail-0123456789abcdef-home": {}, "jail-fedcba9876543210-home": {}, "my-data": {}
        })
        .to_string(),
    )
    .unwrap();
    succeeds(&f.run(&["codex", "--version"]));
    fs::write(
        f.tmp.path().join("images.json"),
        serde_json::json!({
            &used_image: {}, "jail-runtime:0123456789abcdef0123": {},
            "jail-runtime:fedcba9876543210fedc": {}, "docker.io/library/node:22": {}
        })
        .to_string(),
    )
    .unwrap();
    for args in [
        vec!["--dry-run", "cleanup", "--all-delete"],
        vec![
            "cleanup",
            "--all-delete",
            "volume",
            "jail-0123456789abcdef-home",
        ],
    ] {
        assert!(!f.run(&args).status.success(), "{args:?}");
    }
    assert_eq!(items("volume").as_object().unwrap().len(), 4);

    // An active session refuses the deletion before anything is removed.
    let mut session = f
        .command(&["codex", "exec", "long session"])
        .env("JAIL_TEST_SLEEP", "2")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f
        .calls()
        .iter()
        .any(|a| a.iter().any(|s| s == "long session"))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = f.run(&["cleanup", "--all-delete"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("active sessions"));
    assert!(session.wait().unwrap().success());
    assert!(!f
        .calls()
        .iter()
        .any(|a| a[0] == "delete" || a.get(1).is_some_and(|s| s == "delete")));
    assert_eq!(items("volume").as_object().unwrap().len(), 4);

    // The container in use is deleted first, so nothing is skipped.
    let container = f.record()["id"].as_str().unwrap().to_owned();
    let out = f.run(&["cleanup", "--all-delete"]);
    succeeds(&out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(&format!("removed container {container}")));
    assert!(stdout.contains("removed 7 in total"));
    assert!(!stdout.contains("skipped"));
    let containers: Value =
        serde_json::from_slice(&fs::read(f.tmp.path().join("containers.json")).unwrap()).unwrap();
    assert_eq!(containers, serde_json::json!([]));
    let projects = f.tmp.path().join("state/projects");
    for project in fs::read_dir(projects).unwrap() {
        assert!(!project.unwrap().path().join("record.json").exists());
    }
    assert_eq!(
        items("volume")
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["my-data"]
    );
    assert_eq!(
        items("image")
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["docker.io/library/node:22"]
    );
    for call in f.calls() {
        if ["volume", "image"].contains(&call[0].as_str()) {
            assert_ne!(call[1], "prune");
            if call[1] == "delete" {
                assert_eq!(call.len(), 3, "{call:?}");
            }
        }
    }
}

#[test]
fn typed_slash_login_is_refused_and_session_and_terminal_survive() {
    for (tool, host_login) in [("claude", "claude auth login"), ("codex", "codex login")] {
        let f = Fixture::new();
        // Keep this PTY test independent of the host's default service ports.
        fs::write(
            &f.config,
            "[auth]\nimport_host=false\n[network]\npublished_ports=[]\n",
        )
        .unwrap();
        // Any host CLI started by jail would leave this marker.
        let bin = f.tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = f.tmp.path().join("host-cli-ran");
        fs::write(
            bin.join(tool),
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(bin.join(tool), fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = f.command(&[tool]);
        command
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("JAIL_TEST_HOST_LOGIN", host_login);
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/login_pty.py"
            ))
            .arg(command.get_program())
            .args(command.get_args())
            .current_dir(command.get_current_dir().unwrap())
            .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
            .output()
            .unwrap();
        succeeds(&output);
        assert!(!marker.exists());
        let calls = f.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|a| a.iter().any(|s| s == "launch"))
                .count(),
            1
        );
        assert!(!calls.iter().any(|a| a.iter().any(|s| s.ends_with("-auth"))));
    }
}

#[test]
fn home_workspace_is_rejected_before_contacting_backend() {
    let f = Fixture::new();
    let output = f
        .command(&["codex", "--version"])
        .env("HOME", f.tmp.path().join("project with spaces"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("home directory"));
    assert!(f.calls().is_empty());
}

#[test]
fn reuse_literal_arguments_and_exit_code() {
    let f = Fixture::new();
    let prompt = "a prompt; $(touch /tmp/jail-should-not-exist) `whoami`";
    succeeds(&f.run(&["codex", "exec", prompt]));
    succeeds(&f.run(&["claude", "--resume"]));
    let calls = f.calls();
    assert_eq!(calls.iter().filter(|a| a[0] == "run").count(), 1);
    let run = calls.iter().find(|a| a[0] == "run").unwrap();
    let name = &run[run.iter().position(|a| a == "--name").unwrap() + 1];
    assert!(name.starts_with("jail-project-with-spaces-"));
    let published: Vec<_> = run
        .windows(2)
        .filter(|w| w[0] == "--publish")
        .map(|w| &w[1])
        .collect();
    assert_eq!(published.len(), 4);
    assert!(published.iter().all(|p| p.starts_with("127.0.0.1:")));
    assert!(calls
        .iter()
        .any(|a| a.last().map(String::as_str) == Some(prompt)));
    assert!(calls
        .iter()
        .any(|a| a.last().map(String::as_str) == Some("--resume")));
    let record = f.record();
    assert_eq!(record["id"], *name);
    assert_eq!(record["ports"].as_array().unwrap().len(), 4);
    assert_eq!(f.container(name)["status"]["state"], "running");
    let output = f
        .command(&["codex", "exec", "hello"])
        .env("JAIL_TEST_EXIT", "19")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(19));
}

#[test]
fn edit_off_creates_read_only_mount_and_rejects_override() {
    let f = Fixture::new();
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[tools]\nedit=false\n",
    )
    .unwrap();
    succeeds(&f.run(&["claude", "-p", "read this"]));
    let calls = f.calls();
    let run = calls.iter().find(|a| a[0] == "run").unwrap();
    assert!(run
        .iter()
        .any(|a| a.starts_with("type=bind,") && a.ends_with(",readonly")));
    let result = f.run(&["codex", "--config", "features.shell_tool=true"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("override tool policy"));
}

#[test]
fn allow_write_sends_writable_paths_to_the_guest_and_conflicts_with_read_only() {
    let f = Fixture::new();
    let workspace = f.tmp.path().join("project with spaces");
    fs::create_dir(workspace.join("src")).unwrap();
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[workspace]\nallow_write=['src/', 'out/']\n",
    )
    .unwrap();
    succeeds(&f.run(&["claude", "-p", "edit src"]));
    let calls = f.calls();
    let run = calls.iter().find(|a| a[0] == "run").unwrap();
    // The host shares the workspace writable; the guest makes the rest read-only.
    assert!(run
        .iter()
        .any(|a| a.starts_with("type=bind,") && !a.ends_with(",readonly")));
    let guard: Value = serde_json::from_str(
        run.iter()
            .find_map(|a| a.strip_prefix("JAIL_WRITE_GUARD="))
            .unwrap(),
    )
    .unwrap();
    let root = workspace.canonicalize().unwrap();
    assert_eq!(guard["limit"]["root"], root.to_str().unwrap());
    assert_eq!(
        guard["limit"]["writable"],
        serde_json::json!([root.join("out"), root.join("src")])
    );
    assert!(workspace.join("out").is_dir());
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[workspace]\nread_only=true\nallow_write=['src/']\n",
    )
    .unwrap();
    let result = f.run(&["claude", "--version"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("allow_write"));
}

#[test]
fn auto_stop_restarts_and_configuration_changes_require_explicit_recreate() {
    let f = Fixture::new();
    succeeds(&f.run(&["--auto-stop", "codex", "exec", "hello"]));
    let first_calls = f.calls();
    let first = first_calls.iter().find(|a| a[0] == "run").unwrap();
    let first_name = first[first.iter().position(|a| a == "--name").unwrap() + 1].clone();
    let first_volume = first
        .iter()
        .find(|a| a.starts_with("type=volume,"))
        .unwrap()
        .clone();
    assert!(f.calls().iter().any(|a| a[0] == "stop"));
    succeeds(&f.run(&["codex", "exec", "again"]));
    assert!(f.calls().iter().any(|a| a[0] == "start"));
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[resources]\ncpus=2\n",
    )
    .unwrap();
    let output = f.run(&["claude"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("jail recreate"));
    assert!(!f.calls().iter().any(|a| a[0] == "delete"));
    succeeds(&f.run(&["recreate"]));
    succeeds(&f.run(&["claude"]));
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 2);
    let calls = f.calls();
    let second = calls.iter().filter(|a| a[0] == "run").nth(1).unwrap();
    assert_ne!(
        second[second.iter().position(|a| a == "--name").unwrap() + 1],
        first_name
    );
    assert!(second.contains(&first_volume));
}

#[test]
fn auto_stop_does_not_stop_another_session_and_stop_detects_inherited_lock() {
    let f = Fixture::new();
    let mut other = f
        .command(&["codex", "exec", "long session"])
        .env("JAIL_TEST_SLEEP", "2")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f
        .calls()
        .iter()
        .any(|a| a.iter().any(|s| s == "long session"))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    succeeds(&f.run(&["--auto-stop", "claude", "-p", "short session"]));
    assert!(!f.calls().iter().any(|a| a[0] == "stop"));
    let stop = f.run(&["stop"]);
    assert!(!stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stderr).contains("active sessions"));
    assert!(other.wait().unwrap().success());
    succeeds(&f.run(&["stop"]));
}

#[test]
fn dry_run_never_contacts_backend_or_imports_credentials() {
    let f = Fixture::new();
    fs::write(&f.config, "[auth]\nimport_host=true\n").unwrap();
    let out = f.run(&["--dry-run", "claude", "--resume"]);
    succeeds(&out);
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["tool"], "claude");
    assert!(plan["container"]
        .as_str()
        .unwrap()
        .starts_with("jail-project-with-spaces-"));
    assert!(plan["project_key"].as_str().unwrap().starts_with("jail-"));
    assert_eq!(
        plan["network"]["published_ports"],
        serde_json::json!([3000, 5173, 8000, 8080])
    );
    assert!(f.calls().is_empty());
}

#[test]
fn ephemeral_session_mounts_nothing_and_is_deleted_on_exit() {
    let f = Fixture::new();
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[network]\npublished_ports=[]\n[workspace]\nhidden=['.env']\nmounts=[{source='.',target='/extra/config'}]\n",
    )
    .unwrap();
    let out = f.run(&["--dry-run", "--ephemeral", "claude"]);
    succeeds(&out);
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(plan["container"]
        .as_str()
        .unwrap()
        .starts_with("jail-ephemeral-"));
    assert!(plan["workspace"].is_null() && plan["home_volume"].is_null());
    assert_eq!(plan["cwd"], "/home/jail");
    let out = f
        .command(&["--ephemeral", "codex", "exec", "hi"])
        .env("JAIL_TEST_EXIT", "7")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    let calls = f.calls();
    let run = calls.iter().find(|a| a[0] == "run").unwrap();
    assert!(!run.iter().any(|a| a == "--mount" || a == "--masked-path"));
    assert!(run.iter().any(|a| a == "io.jail.ephemeral=true"));
    assert!(!calls.iter().any(|a| a[0] == "volume"));
    let exec = calls
        .iter()
        .find(|a| a[0] == "exec" && a.iter().any(|s| s == "launch"))
        .unwrap();
    assert!(exec.windows(2).any(|w| w == ["--workdir", "/home/jail"]));
    // The container and its jail state are gone; the next launch is fresh.
    let state: Value =
        serde_json::from_slice(&fs::read(f.tmp.path().join("containers.json")).unwrap()).unwrap();
    assert_eq!(state, serde_json::json!([]));
    let projects = f.tmp.path().join("state/projects");
    assert_eq!(fs::read_dir(projects).unwrap().count(), 0);
    for args in [vec!["--ephemeral", "stop"], vec!["--ephemeral", "recreate"]] {
        assert!(!f.run(&args).status.success(), "{args:?}");
    }
}

#[test]
fn occupied_host_port_is_remapped_and_reported_but_not_silently_changed_on_restart() {
    use std::net::TcpListener;
    let f = Fixture::new();
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let requested = busy.local_addr().unwrap().port();
    fs::write(
        &f.config,
        format!("[auth]\nimport_host=false\n[network]\npublished_ports=[{requested}]\n"),
    )
    .unwrap();
    let output = f.run(&["codex", "--version"]);
    succeeds(&output);
    let port = &f.record()["ports"][0];
    assert_eq!(port["container"], requested);
    let assigned = port["host"].as_u64().unwrap() as u16;
    assert_ne!(assigned, requested);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("http://127.0.0.1:{assigned}"))
    );
    succeeds(&f.run(&["stop"]));
    let conflict = TcpListener::bind(("127.0.0.1", assigned)).unwrap();
    let output = f.run(&["claude", "--version"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("saved host port"));
    assert!(!f.calls().iter().any(|a| a[0] == "start"));
    drop(conflict);
    succeeds(&f.run(&["claude", "--version"]));
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 1);
}

#[test]
fn port_forwarding_can_be_disabled_and_strict_conflicts_never_launch() {
    use std::net::TcpListener;
    for network in ["published_ports=[]", "mode='none'"] {
        let f = Fixture::new();
        fs::write(
            &f.config,
            format!("[auth]\nimport_host=false\n[network]\n{network}\n"),
        )
        .unwrap();
        succeeds(&f.run(&["codex", "--version"]));
        assert!(!f
            .calls()
            .iter()
            .find(|a| a[0] == "run")
            .unwrap()
            .contains(&"--publish".to_owned()));
    }
    let f = Fixture::new();
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    fs::write(
        &f.config,
        format!(
            "[auth]\nimport_host=false\n[network]\npublished_ports=[{port}]\nauto_port=false\n"
        ),
    )
    .unwrap();
    let output = f.run(&["codex", "--version"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    assert!(!f
        .calls()
        .iter()
        .any(|a| a[0] == "run" || a.iter().any(|s| s == "launch")));
}

#[test]
fn legacy_names_use_stable_metadata_and_session_locks() {
    let f = Fixture::new();
    fs::write(
        &f.config,
        "[auth]\nimport_host=false\n[network]\npublished_ports=[]\n",
    )
    .unwrap();
    let plan: Value =
        serde_json::from_slice(&f.run(&["--dry-run", "codex", "--version"]).stdout).unwrap();
    succeeds(&f.run(&["codex", "--version"]));
    let key = plan["project_key"].as_str().unwrap();
    let path = f
        .tmp
        .path()
        .join("state/projects")
        .join(key)
        .join("record.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["id"] = key.into();
    record.as_object_mut().unwrap().remove("ports");
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let containers = f.tmp.path().join("containers.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&containers).unwrap()).unwrap();
    state[0]["configuration"]["id"] = key.into();
    fs::write(containers, serde_json::to_vec(&state).unwrap()).unwrap();
    succeeds(&f.run(&["claude", "--version"]));
    succeeds(&f.run(&["stop", "--all"]));
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 1);
    assert_eq!(f.record()["id"], key);
    assert_eq!(f.container(key)["status"]["state"], "stopped");
}
