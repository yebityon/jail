use crate::config::Config;
use anyhow::{bail, Result};

/// Tool restrictions are defense in depth. VM mounts and firewall are the hard boundary.
pub fn agent_args(tool: &str, args: &[String], c: &Config) -> Result<Vec<String>> {
    if (tool == "codex"
        && (args.first().is_some_and(|a| a == "logout")
            || (args.first().is_some_and(|a| a == "login")
                && args.get(1).map(String::as_str) != Some("status"))))
        || (tool == "claude"
            && (args.first().is_some_and(|a| a == "setup-token")
                || (args.first().is_some_and(|a| a == "auth")
                    && args.get(1).map(String::as_str) != Some("status"))))
    {
        bail!(
            "sign in or out on the host, then use jail auth; jail supports subscription login only"
        );
    }
    let restrictions = !c.tools.claude_deny.is_empty()
        || !c.tools.codex_disabled_features.is_empty()
        || !c.tools.shell
        || !c.tools.edit
        || !c.tools.web_search
        || !c.tools.mcp
        || !c.tools.hooks;
    if restrictions {
        for arg in args {
            let flag = arg.split('=').next().unwrap_or(arg);
            let blocked = match tool {
                "claude" => [
                    "--tools",
                    "--allowedTools",
                    "--allowed-tools",
                    "--disallowedTools",
                    "--disallowed-tools",
                    "--settings",
                    "--setting-sources",
                    "--mcp-config",
                    "--strict-mcp-config",
                    "--plugin-dir",
                    "--plugin-url",
                    "--agents",
                    "--agent",
                    "--dangerously-skip-permissions",
                    "--allow-dangerously-skip-permissions",
                    "--permission-mode",
                    "--remote-control",
                    "--chrome",
                    "--cloud",
                    "--environment",
                ]
                .contains(&flag),
                "codex" => {
                    [
                        "-c",
                        "--config",
                        "--enable",
                        "--disable",
                        "-p",
                        "--profile",
                        "--remote",
                        "--remote-auth-token-env",
                        "--dangerously-bypass-approvals-and-sandbox",
                        "--dangerously-bypass-hook-trust",
                        "--search",
                        "--oss",
                        "--local-provider",
                    ]
                    .contains(&flag)
                        || (flag.starts_with("-c") && !flag.starts_with("--"))
                        || (flag.starts_with("-p") && !flag.starts_with("--"))
                }
                _ => bail!("only claude and codex are supported"),
            };
            if blocked {
                bail!("{flag} can override tool policy; enable the corresponding features in jail's config instead");
            }
        }
        // Commands that launch a different executor are not covered by the tool policy.
        let subcommand = args.first().map(String::as_str).unwrap_or("");
        if (tool == "codex"
            && [
                "sandbox",
                "app-server",
                "exec-server",
                "remote-control",
                "app",
                "cloud",
            ]
            .contains(&subcommand))
            || (tool == "claude" && ["remote-control", "daemon", "attach"].contains(&subcommand))
        {
            bail!("{subcommand} is unavailable while tool restrictions are enabled");
        }
    }
    let mut out = Vec::new();
    if tool == "claude" {
        let mut deny = vec![];
        deny.extend(c.tools.claude_deny.iter().map(String::as_str));
        if !c.tools.shell {
            deny.extend(["Bash", "PowerShell", "REPL", "Agent", "Task"]);
        }
        if !c.tools.edit {
            deny.extend(["Edit", "Write", "NotebookEdit"]);
        }
        if !c.tools.web_search {
            deny.extend(["WebSearch", "WebFetch"]);
        }
        if !c.tools.mcp {
            deny.push("mcp__*");
            out.extend([
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                "{\"mcpServers\":{}}".into(),
            ]);
        }
        if !deny.is_empty() {
            out.extend(["--disallowedTools".into(), deny.join(",")]);
        }
        // Do not import host settings; project instructions remain available.
        out.extend(["--settings".into(), "/etc/jail/claude-settings.json".into()]);
    } else if tool == "codex" {
        for setting in [
            format!("features.shell_tool={}", c.tools.shell),
            format!("features.unified_exec={}", c.tools.shell),
            format!("features.multi_agent={}", c.tools.shell),
            format!("features.hooks={}", c.tools.hooks),
            format!(
                "web_search=\"{}\"",
                if c.tools.web_search {
                    "live"
                } else {
                    "disabled"
                }
            ),
            "cli_auth_credentials_store=\"file\"".into(),
            "chatgpt_base_url=\"http://127.0.0.1:3131/backend-api\"".into(),
            "model_provider=\"jail_oauth\"".into(),
            "model_providers.jail_oauth.name=\"Jail subscription gateway\"".into(),
            "model_providers.jail_oauth.base_url=\"http://127.0.0.1:3131/backend-api/codex\""
                .into(),
            "model_providers.jail_oauth.wire_api=\"responses\"".into(),
            "model_providers.jail_oauth.requires_openai_auth=true".into(),
            "model_providers.jail_oauth.supports_websockets=false".into(),
            "features.plugins=false".into(),
            "features.apps=false".into(),
        ] {
            out.extend(["-c".into(), setting]);
        }
        for feature in &c.tools.codex_disabled_features {
            out.extend(["--disable".into(), feature.clone()]);
        }
        if !c.tools.edit || c.workspace.read_only {
            out.extend(["--sandbox".into(), "read-only".into()]);
        }
    } else {
        bail!("only claude and codex are supported");
    }
    out.extend_from_slice(args);
    Ok(out)
}

pub fn claude_settings(c: &Config) -> serde_json::Value {
    let mut deny = vec![];
    deny.extend(c.tools.claude_deny.iter().map(String::as_str));
    if !c.tools.shell {
        deny.extend(["Bash", "PowerShell", "REPL", "Agent", "Task"]);
    }
    if !c.tools.edit {
        deny.extend(["Edit", "Write", "NotebookEdit"]);
    }
    if !c.tools.web_search {
        deny.extend(["WebSearch", "WebFetch"]);
    }
    if !c.tools.mcp {
        deny.push("mcp__*");
    }
    serde_json::json!({
        "permissions": { "deny": deny },
        "disableAllHooks": !c.tools.hooks,
        "allowManagedPermissionRulesOnly": true,
        "allowManagedHooksOnly": !c.tools.hooks,
        "syncClaudeAiSkills": false,
        "syncClaudeAiPlugins": false,
        "enabledPlugins": {},
        "env": {"ANTHROPIC_BASE_URL":"http://127.0.0.1:3130"},
        "autoUpdatesChannel": "stable"
    })
}

pub fn codex_requirements(c: &Config) -> String {
    // Restrict configured MCP servers even if project config adds them.
    let mut text = String::from("chatgpt_base_url = \"http://127.0.0.1:3131/backend-api\"\n");
    if !c.tools.edit || c.workspace.read_only {
        text.push_str("allowed_sandbox_modes = [\"read-only\"]\n");
    }
    if !c.tools.web_search {
        text.push_str("allowed_web_search_modes = [\"disabled\"]\n");
    }
    // Codex normalizes unified_exec back on unless managed requirements pin it.
    // Pin all disabled features so in-session configuration cannot restore them.
    let mut features = std::collections::BTreeMap::from([
        ("apps".to_owned(), false),
        ("plugins".to_owned(), false),
    ]);
    if !c.tools.shell {
        for name in ["shell_tool", "unified_exec", "multi_agent"] {
            features.insert(name.to_owned(), false);
        }
    }
    if !c.tools.hooks {
        features.insert("hooks".to_owned(), false);
    }
    for name in &c.tools.codex_disabled_features {
        features.insert(name.clone(), false);
    }
    text.push_str("\n[features]\n");
    text.push_str(&toml::to_string(&features).expect("string/bool map is valid TOML"));
    if !c.tools.mcp {
        text.push_str("\n[mcp_servers]\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_args_keep_prompt_literal() {
        let c = Config::default();
        let prompt = "spaces; $(touch /tmp/escaped) `whoami`".to_owned();
        assert_eq!(
            agent_args("codex", &["exec".into(), prompt.clone()], &c)
                .unwrap()
                .last(),
            Some(&prompt)
        );
    }
    #[test]
    fn policy_overrides_rejected() {
        let c = Config::default();
        for args in [
            vec!["--config=features.shell_tool=true".into()],
            vec!["-cfeatures.shell_tool=true".into()],
            vec!["--enable".into(), "apps".into()],
        ] {
            assert!(agent_args("codex", &args, &c).is_err());
        }
        assert!(agent_args("claude", &["--tools=default".into()], &c).is_err());
    }
    #[test]
    fn disabled_codex_features_are_managed_requirements() {
        let mut c = Config::default();
        c.tools.shell = false;
        c.tools.codex_disabled_features.push("goals".into());
        let requirements: toml::Value = codex_requirements(&c).parse().unwrap();
        for name in [
            "shell_tool",
            "unified_exec",
            "multi_agent",
            "hooks",
            "goals",
        ] {
            assert_eq!(requirements["features"][name].as_bool(), Some(false));
        }
        assert!(requirements["mcp_servers"].as_table().unwrap().is_empty());
    }
}
