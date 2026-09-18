//! Render a [`Config`] into the exact argv and environment of a child.
//!
//! Kept separate from spawning so a caller that owns its own process
//! handling can still use the typed config: the kernel's runner drives the
//! child through an Erlang `Port` because it needs the stdin channel, and
//! `argv`/`env` are all it wants from this crate.

use std::collections::BTreeMap;

use crate::config::{Config, Error, Features, InputFormat, McpPolicy, OutputFormat, SessionMode};

/// An MCP configuration naming no servers, the payload of
/// [`McpPolicy::None`].
const NO_MCP_SERVERS: &str = r#"{"mcpServers":{}}"#;

/// The environment variable that stops a child spawning built-in subagents.
const DISABLE_BUILTIN_AGENTS: &str = "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS";

impl Config {
    /// The child's argv, the executable included at index 0.
    ///
    /// Always a print run: this crate exists for headless children, and an
    /// interactive session has no argv worth rendering.
    ///
    /// The prompt, when there is one, sits immediately after `-p` rather
    /// than at the end. Several of the CLI's options are variadic
    /// (`--allowedTools`, `--tools`, `--add-dir`, `--mcp-config`), and a
    /// variadic option swallows every following token that does not start
    /// with `-`, so a trailing positional prompt would be eaten by whichever
    /// list happened to be rendered last.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the combination is one the CLI rejects; see
    /// [`Config::validate`] for the list.
    pub fn argv(&self) -> Result<Vec<String>, Error> {
        self.validate()?;
        let mut argv = vec![
            self.bin
                .as_ref()
                .map_or_else(|| "claude".to_owned(), |bin| bin.display().to_string()),
        ];
        argv.extend(self.launcher_args.iter().cloned());
        argv.push("-p".to_owned());
        if let Some(prompt) = &self.prompt {
            argv.push(prompt.clone());
        }
        option(
            &mut argv,
            "--output-format",
            self.output_format.as_str().to_owned(),
        );
        option(
            &mut argv,
            "--input-format",
            self.input_format.as_str().to_owned(),
        );
        if matches!(self.output_format, OutputFormat::StreamJson) {
            // The CLI refuses `-p --output-format stream-json` without it,
            // so this is not a caller's choice to get wrong.
            flag(&mut argv, "--verbose");
        }
        if let Some(model) = &self.model {
            option(&mut argv, "--model", model.clone());
        }
        if !self.fallback_models.is_empty() {
            option(
                &mut argv,
                "--fallback-model",
                self.fallback_models.join(","),
            );
        }
        if let Some(mode) = self.permission_mode {
            option(&mut argv, "--permission-mode", mode.as_str().to_owned());
        }
        if let Some(effort) = self.effort {
            option(&mut argv, "--effort", effort.as_str().to_owned());
        }
        if let Some(tools) = &self.allowed_tools {
            option(&mut argv, "--allowedTools", tools.join(","));
        }
        if let Some(tools) = &self.disallowed_tools {
            option(&mut argv, "--disallowedTools", tools.join(","));
        }
        if let Some(tools) = &self.tools {
            option(&mut argv, "--tools", tools.join(","));
        }
        for dir in &self.add_dirs {
            option(&mut argv, "--add-dir", dir.display().to_string());
        }
        push_session(&mut argv, &self.session);
        push_mcp(&mut argv, &self.mcp);
        if let Some(prompt) = &self.system_prompt {
            option(&mut argv, "--system-prompt", prompt.clone());
        }
        if let Some(prompt) = &self.append_system_prompt {
            option(&mut argv, "--append-system-prompt", prompt.clone());
        }
        if let Some(settings) = &self.settings {
            option(&mut argv, "--settings", settings.clone());
        }
        if let Some(sources) = &self.setting_sources {
            let joined = sources
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(",");
            option(&mut argv, "--setting-sources", joined);
        }
        if let Some(budget) = self.max_budget_usd {
            option(&mut argv, "--max-budget-usd", budget.to_string());
        }
        push_features(&mut argv, &self.features);
        Ok(argv)
    }

    /// The environment entries to overlay on the caller's own.
    ///
    /// [`Config::env`] wins over anything a feature implies, so a caller can
    /// always override.
    #[must_use]
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if !self.features.builtin_agents {
            env.insert(DISABLE_BUILTIN_AGENTS.to_owned(), "1".to_owned());
        }
        env.extend(self.env.clone());
        env
    }

    /// Refuse the combinations the CLI rejects, or accepts and then ignores.
    ///
    /// Every one of these is a mistake that currently surfaces as a usage
    /// message on the child's stderr, several seconds and one process
    /// later, or (worse) as a flag that is quietly inert.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], naming the field and what to set instead.
    pub fn validate(&self) -> Result<(), Error> {
        let streaming = matches!(self.output_format, OutputFormat::StreamJson);
        let stdin_turns = matches!(self.input_format, InputFormat::StreamJson);
        let features = &self.features;

        refuse(
            self.prompt.is_some() && stdin_turns,
            "prompt is set with input_format = StreamJson; under stream-json input the prompt \
             is a stdin line, and an argv positional would be swallowed by a preceding \
             variadic flag. Clear `prompt` and write the first user turn to stdin.",
        )?;
        refuse(
            self.prompt.is_none() && !stdin_turns,
            "no prompt with input_format = Text; the child would read one from stdin, which \
             this crate closes. Set `prompt`, or switch to input_format = StreamJson.",
        )?;
        for (enabled, field, flag) in [
            (
                features.partial_messages,
                "partial_messages",
                "--include-partial-messages",
            ),
            (features.hook_events, "hook_events", "--include-hook-events"),
            (
                features.subagent_text,
                "subagent_text",
                "--forward-subagent-text",
            ),
            (
                features.replay_user_messages,
                "replay_user_messages",
                "--replay-user-messages",
            ),
        ] {
            refuse(
                enabled && !streaming,
                &format!(
                    "features.{field} renders {flag}, which the CLI only honours with \
                     output_format = StreamJson"
                ),
            )?;
        }
        refuse(
            features.replay_user_messages && !stdin_turns,
            "features.replay_user_messages echoes stdin turns back, so it needs \
             input_format = StreamJson",
        )?;
        refuse(
            features.fork_session
                && matches!(self.session, SessionMode::New | SessionMode::NewWithId(_)),
            "features.fork_session only means something when resuming; set \
             session = Resume(id) or Continue, or turn it off",
        )?;
        refuse(
            matches!(
                &self.mcp,
                McpPolicy::Only { servers, configs } if servers.is_empty() && configs.is_empty()
            ),
            "McpPolicy::Only with no servers and no configs renders --strict-mcp-config with \
             nothing for it to be strict about; use McpPolicy::None, which passes an empty \
             server map",
        )
    }
}

/// Refuse when `wrong` holds.
///
/// A list of these reads as the list of refusals it is; the same thing
/// written as a ladder of early returns is one statement shape repeated
/// seven times, which the repo's clone gate refuses (and was right to).
fn refuse(wrong: bool, message: &str) -> Result<(), Error> {
    if wrong {
        return Err(Error::Config {
            message: message.to_owned(),
        });
    }
    Ok(())
}

/// Which conversation the run belongs to.
fn push_session(argv: &mut Vec<String>, session: &SessionMode) {
    match session {
        SessionMode::New => {}
        SessionMode::NewWithId(id) => option(argv, "--session-id", id.clone()),
        SessionMode::Resume(id) => option(argv, "--resume", id.clone()),
        SessionMode::Continue => flag(argv, "--continue"),
    }
}

/// Which MCP servers the child may reach.
fn push_mcp(argv: &mut Vec<String>, mcp: &McpPolicy) {
    match mcp {
        McpPolicy::Inherit => {}
        McpPolicy::None => {
            flag(argv, "--strict-mcp-config");
            option(argv, "--mcp-config", NO_MCP_SERVERS.to_owned());
        }
        McpPolicy::Only { servers, configs } => {
            flag(argv, "--strict-mcp-config");
            if !servers.is_empty() {
                option(
                    argv,
                    "--mcp-config",
                    crate::config::mcp_config_json(servers),
                );
            }
            for config in configs {
                option(argv, "--mcp-config", config.clone());
            }
        }
    }
}

/// The feature toggles that render as a flag. `builtin_agents` is missing
/// on purpose: it is an environment entry, not an option (see
/// [`Config::env`]).
fn push_features(argv: &mut Vec<String>, features: &Features) {
    for (enabled, name) in [
        (features.partial_messages, "--include-partial-messages"),
        (features.hook_events, "--include-hook-events"),
        (features.subagent_text, "--forward-subagent-text"),
        (features.replay_user_messages, "--replay-user-messages"),
        (features.fork_session, "--fork-session"),
        (!features.session_persistence, "--no-session-persistence"),
        (!features.slash_commands, "--disable-slash-commands"),
        (features.bare, "--bare"),
        (features.safe_mode, "--safe-mode"),
        (features.skip_permissions, "--dangerously-skip-permissions"),
    ] {
        if enabled {
            flag(argv, name);
        }
    }
}

/// Push a valueless flag.
fn flag(argv: &mut Vec<String>, name: &str) {
    argv.push(name.to_owned());
}

/// Push a flag and its single value.
///
/// Every list-valued option renders as one comma-joined token rather than
/// several: the CLI accepts both spellings, and one token cannot be
/// mistaken for the start of a positional argument.
fn option(argv: &mut Vec<String>, name: &str, value: String) {
    argv.push(name.to_owned());
    argv.push(value);
}

#[cfg(test)]
mod tests {
    use crate::config::{
        Config, Effort, Features, InputFormat, McpPolicy, McpServer, OutputFormat, PermissionMode,
        SessionMode,
    };

    /// The index of `needle` in `argv`.
    fn at(argv: &[String], needle: &str) -> Option<usize> {
        argv.iter().position(|arg| arg == needle)
    }

    #[test]
    fn launcher_args_come_before_the_print_flag() {
        // The executable can be a wrapper that execs claude itself; its own
        // arguments are not claude's.
        let mut config = Config::print("hi");
        config.launcher_args = vec!["vm-1".to_owned(), "/work".to_owned()];
        config.bin = Some("remote-claude".into());
        let argv = config.argv().expect("valid config");
        assert_eq!(argv[..5], ["remote-claude", "vm-1", "/work", "-p", "hi"]);
    }

    #[test]
    fn a_prompt_never_follows_a_variadic_flag() {
        // The failure this guards is silent: commander collects every
        // following non-`-` token into the variadic list, so a trailing
        // prompt becomes a tool name and the run gets no instructions.
        let argv = Config::print("do the thing")
            .allowed_tools(["Read"])
            .argv()
            .expect("valid config");
        let print = at(&argv, "-p").expect("a print run");
        assert_eq!(argv[print + 1], "do the thing");
        assert!(at(&argv, "--allowedTools").expect("rendered") > print + 1);
    }

    #[test]
    fn an_empty_allowlist_is_not_an_absent_one() {
        let absent = Config::print("hi").argv().expect("valid config");
        assert_eq!(at(&absent, "--allowedTools"), None);

        let empty = Config::print("hi")
            .allowed_tools(Vec::<String>::new())
            .argv()
            .expect("valid config");
        let flag = at(&empty, "--allowedTools").expect("rendered");
        assert_eq!(empty[flag + 1], "");
    }

    #[test]
    fn stream_json_output_forces_verbose() {
        // The CLI refuses `-p --output-format stream-json` without it.
        let argv = Config::print("hi").argv().expect("valid config");
        assert!(at(&argv, "--verbose").is_some());
    }

    #[test]
    fn no_mcp_renders_the_lockdown_pair() {
        let argv = Config::print("hi")
            .mcp(McpPolicy::None)
            .argv()
            .expect("valid config");
        assert!(at(&argv, "--strict-mcp-config").is_some());
        let flag = at(&argv, "--mcp-config").expect("rendered");
        assert_eq!(argv[flag + 1], r#"{"mcpServers":{}}"#);
    }

    #[test]
    fn every_scalar_choice_renders_its_cli_spelling() {
        let mut config = Config::print("hi").permission_mode(PermissionMode::BypassPermissions);
        config.effort = Some(Effort::Xhigh);
        config.model = Some("opus".to_owned());
        config.session = SessionMode::Resume("abc".to_owned());
        let argv = config.argv().expect("valid config");
        for (flag, value) in [
            ("--permission-mode", "bypassPermissions"),
            ("--effort", "xhigh"),
            ("--model", "opus"),
            ("--resume", "abc"),
        ] {
            let index = at(&argv, flag).unwrap_or_else(|| panic!("{flag} rendered"));
            assert_eq!(argv[index + 1], value);
        }
    }

    #[test]
    fn disabling_builtin_agents_is_an_env_entry_not_a_flag() {
        let mut config = Config::print("hi");
        config.features.builtin_agents = false;
        assert_eq!(
            config.env().get("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"),
            Some(&"1".to_owned())
        );
        assert!(
            !config
                .argv()
                .expect("valid config")
                .iter()
                .any(|arg| arg.contains("BUILTIN_AGENTS"))
        );
    }

    #[test]
    fn a_caller_env_entry_wins_over_a_feature() {
        let mut config = Config::print("hi");
        config.features.builtin_agents = false;
        config.env.insert(
            "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS".to_owned(),
            "0".to_owned(),
        );
        assert_eq!(
            config.env().get("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"),
            Some(&"0".to_owned())
        );
    }

    #[test]
    fn a_prompt_under_stream_json_input_is_refused() {
        let mut config = Config::print("hi");
        config.input_format = InputFormat::StreamJson;
        let message = config.validate().expect_err("refused").to_string();
        assert!(message.contains("stdin line"), "{message}");
    }

    #[test]
    fn a_missing_prompt_under_text_input_is_refused() {
        let config = Config::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_streaming_only_feature_under_text_output_is_refused() {
        let mut config = Config::print("hi");
        config.output_format = OutputFormat::Text;
        config.features = Features {
            partial_messages: true,
            ..Features::default()
        };
        let message = config.validate().expect_err("refused").to_string();
        assert!(message.contains("--include-partial-messages"), "{message}");
    }

    #[test]
    fn forking_without_a_session_to_fork_is_refused() {
        let mut config = Config::print("hi");
        config.features.fork_session = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn strict_mcp_with_nothing_to_be_strict_about_is_refused() {
        let config = Config::print("hi").mcp(McpPolicy::Only {
            servers: Vec::new(),
            configs: Vec::new(),
        });
        let message = config.validate().expect_err("refused").to_string();
        assert!(message.contains("McpPolicy::None"), "{message}");
    }

    #[test]
    fn kernel_only_renders_the_three_flags_that_mean_one_server() {
        // The trio is the whole point: --tools "" removes the built-ins,
        // the generated --mcp-config names the one server, and
        // --strict-mcp-config stops another source adding more.
        let argv = Config::print("hi")
            .kernel_only(McpServer::stdio("index", "/bin/ix-mcp-ex"))
            .argv()
            .expect("valid config");
        assert_eq!(at(&argv, "--tools").map(|i| argv[i + 1].as_str()), Some(""));
        assert!(at(&argv, "--strict-mcp-config").is_some());
        let config = at(&argv, "--mcp-config").expect("rendered");
        assert_eq!(
            argv[config + 1],
            r#"{"mcpServers":{"index":{"args":[],"command":"/bin/ix-mcp-ex","env":{},"type":"stdio"}}}"#
        );
        // The approval axis is untouched: this preset is about what exists.
        assert_eq!(at(&argv, "--allowedTools"), None);
    }

    #[test]
    fn kernel_only_also_closes_the_cli_s_own_subagents() {
        // A child whose only tool is the kernel can still reach the CLI's
        // built-in agent types unless this is off, which would put a second
        // spawn path under the star topology.
        let config = Config::print("hi").kernel_only(McpServer::stdio("index", "/bin/k"));
        assert!(!config.features.builtin_agents);
        assert_eq!(
            config.env().get("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"),
            Some(&"1".to_owned())
        );
    }
}
