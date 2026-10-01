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
    ] {
        assert!(!f.run(&args).status.success());
    }
    fs::write(&f.config, "[environment]\npass=['OPENAI_API_KEY']\n").unwrap();
    assert!(!f.run(&["codex", "--version"]).status.success());
    assert!(f.calls().is_empty());
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
fn tail_alias_and_all_are_read_only_plain_snapshots_when_piped() {
    let f = Fixture::new();
    succeeds(&f.run(&["codex", "--version"]));
    let before = f.calls().len();
    for args in [
        vec!["--tail"],
        vec!["tail"],
        vec!["--tail", "--all"],
        vec!["tail", "--all"],
    ] {
        let output = f.run(&args);
        succeeds(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("JAIL LIVE"));
        assert!(text.contains("[RUNNING]"));
        assert!(text.contains("128.0 MiB / 256.0 MiB (50.0%)"));
        assert!(text.contains("codex"));
        assert!(text.contains("git"));
        assert!(!text.contains("jail-guest"));
        assert!(!text.contains('\x1b'));
    }
    assert!(f.calls()[before..].iter().all(|args| {
        ["list", "stats", "exec"].contains(&args[0].as_str())
            && !args
                .iter()
                .any(|arg| arg == "launch" || arg == "import-auth")
    }));
    succeeds(&f.run(&["stop"]));
    let output = f.run(&["--tail"]);
    succeeds(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("[STOPPED]"));
    assert!(!f.run(&["--tail", "claude"]).status.success());
    assert!(!f.run(&["--auto-stop", "--tail"]).status.success());
}

#[test]
fn tail_redraws_each_second_during_slow_queries_and_restores_terminal() {
    let f = Fixture::new();
    succeeds(&f.run(&["codex", "--version"]));
    let before = f.calls().len();
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tail_pty.py"
        ))
        .args([
            env!("CARGO_BIN_EXE_jail"),
            "--config",
            f.config.to_str().unwrap(),
            "--tail",
        ])
        .current_dir(f.tmp.path().join("project with spaces"))
        .env("JAIL_HOME", f.tmp.path().join("state"))
        .env("JAIL_CONTAINER_BIN", &f.backend)
        .env("JAIL_TEST_BACKEND", f.tmp.path())
        .env("JAIL_TEST_STATS_SLEEP", "2.5")
        .output()
        .unwrap();
    succeeds(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["code"], 0);
    assert_eq!(result["restored"], true, "{result}");
    assert_eq!(result["has_memory"], true);
    assert_eq!(result["has_process"], true);
    assert_eq!(result["has_freshness"], true);
    let frames = result["frames"].as_array().unwrap();
    assert!(frames.len() >= 4, "{result}");
    assert_eq!(frames[0], "00:00:00");
    assert_eq!(frames[1], "00:00:01");
    assert_eq!(frames[2], "00:00:02");
    assert!(!f.calls()[before..]
        .iter()
        .any(|args| ["start", "stop", "run", "delete"].contains(&args[0].as_str())));
}

#[test]
fn reuse_literal_arguments_status_and_exit_code() {
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
    let status = f.run(&["status", "--json"]);
    succeeds(&status);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["containers"][0]["state"], "running");
    assert_eq!(status["containers"][0]["id"], *name);
    assert_eq!(
        status["containers"][0]["ports"].as_array().unwrap().len(),
        4
    );
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
    succeeds(&f.run(&["codex", "--version"]));
    let output = f.run(&["status", "--json"]);
    succeeds(&output);
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    let port = &status["containers"][0]["ports"][0];
    assert_eq!(port["container"], requested);
    let assigned = port["host"].as_u64().unwrap() as u16;
    assert_ne!(assigned, requested);
    let tail = f.run(&["--tail"]);
    succeeds(&tail);
    assert!(String::from_utf8_lossy(&tail.stdout).contains(&format!("http://127.0.0.1:{assigned}")));
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
    succeeds(&f.run(&["--tail"]));
    succeeds(&f.run(&["stop", "--all"]));
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 1);
    let output = f.run(&["status", "--json"]);
    succeeds(&output);
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["containers"][0]["id"], key);
    assert_eq!(status["containers"][0]["state"], "stopped");
}
