//! Elixir binding for the typed Claude Code launcher (ENG-11979).
//!
//! Build the config as a struct in Elixir, launch a headless `claude -p`
//! run, and consume `--output-format stream-json` as typed events instead of
//! lines:
//!
//! ```elixir
//! config = %{ClaudeLaunch.default_config() | prompt: "say hi", tools: []}
//! {:ok, events} = ClaudeLaunch.events(config)
//! Enum.each(events, fn %ClaudeLaunch.Event{kind: kind} -> IO.puts(kind) end)
//! ```
//!
//! # Why so many strings in a crate whose point is types
//!
//! Every enum in `claude-launch-core` (`PermissionMode`, `OutputFormat`,
//! `SessionMode`, `McpPolicy`, `Event`) arrives here as a `String`
//! discriminant plus the union of its payloads. That is not the design: it
//! is unibind's IR, which models records, error enums, objects and streams
//! but has no plain (non-error) enum at all, so a closed set has nowhere to
//! land. ENG-11981 tracks it. Until then the conversions below are the one
//! place the flattening happens, each refusing an unknown spelling with an
//! error that names every accepted value, so a caller in a language without
//! enums can still discover the set.
//!
//! # `ensure_sigchld_default` is required, and darwin will not tell you
//!
//! The BEAM's VM process sets `SIGCHLD` to `SIG_IGN`, so the kernel reaps a
//! NIF's children before anything can read an exit status and `waitpid`
//! answers `ECHILD`. `unibind_ex_runtime::ensure_sigchld_default` restores
//! the default disposition, and the two sibling process-spawning NIFs
//! (`plumb-ex`, `tui-ex`) call it before their first spawn.
//!
//! Skipping it here looks defensible, because `tokio::process` registers
//! its own `SIGCHLD` handler when it creates a `Child` and that displaces
//! `SIG_IGN` too. On darwin the suite passes either way, which is what made
//! the shortcut look safe. It is not: in the Linux build sandbox, without
//! the call, a stub exiting `3` came back as `killed by a signal`, because
//! `wait` failed instead of returning a status. The call stays, and this
//! paragraph is here so nobody re-runs that experiment on a Mac and reaches
//! the same wrong conclusion.
//!
//! It runs *before* the first spawn, so tokio's handler is the one left
//! installed rather than the `SIG_DFL` this writes.

/// Copy a core struct into its exported mirror, or back, field for field.
///
/// Every record below exists only because unibind needs
/// `#[unibind::record]` on a struct declared inside the exported module, so
/// the core type cannot carry it. The copy is mechanical and there are
/// several of them, which is several chances to read a field from the wrong
/// source; declaring the field list once removes them all. Defined here
/// rather than inside the module because `macro_rules!` is textually scoped
/// and the module is a proc-macro input.
macro_rules! mirror {
    ($name:ident: $from:path => $to:path { $($field:ident),+ $(,)? }) => {
        fn $name(source: $from) -> $to {
            $to { $($field: source.$field,)+ }
        }
    };
}

/// The exported boundary. The module name names the generated Elixir
/// namespace (`ClaudeLaunch`) and the OTP app (`:claude_launch`).
#[unibind::export(backends(ex))]
mod _claude_launch {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use claude_launch_core as core;
    use unibind_runtime::UniStream;

    /// Everything the launcher refuses to carry forward.
    ///
    /// A child that simply exits nonzero is not one of these: that arrives
    /// as an `%Event{kind: "exited"}` with the status on it. These are the
    /// four ways a run cannot proceed at all.
    #[unibind::error]
    #[derive(Debug)]
    pub enum ClaudeError {
        /// The config asks for something the CLI rejects, or accepts and
        /// then ignores. Caught before the child starts.
        Config {
            /// What is wrong and what to set instead.
            message: String,
        },
        /// The child could not be started.
        Spawn {
            /// The underlying OS failure.
            message: String,
        },
        /// The child emitted an event kind this crate does not model, with
        /// `strict_protocol` on.
        Protocol {
            /// What arrived, and when the mirror was last checked.
            message: String,
        },
        /// The child ended without a terminal result.
        Exited {
            /// The exit status and the child's stderr.
            message: String,
        },
    }

    unibind_ex_runtime::message_error!(ClaudeError {
        Config,
        Spawn,
        Protocol,
        Exited,
    });

    /// Feature toggles, one named boolean each: "enable these, do not
    /// enable those" as data rather than as flags a caller has to remember.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Features {
        /// Emit partial message chunks as they arrive.
        pub partial_messages: bool,
        /// Emit hook lifecycle events.
        pub hook_events: bool,
        /// Forward subagent text and thinking as messages.
        pub subagent_text: bool,
        /// Echo injected user turns back on stdout.
        pub replay_user_messages: bool,
        /// When resuming, mint a new session id instead of writing back.
        pub fork_session: bool,
        /// Persist the session to disk; false makes the run unresumable.
        pub session_persistence: bool,
        /// Load skills.
        pub slash_commands: bool,
        /// Let the child spawn built-in subagent types. False is the env
        /// half of the kernel's depth-1 lockdown.
        pub builtin_agents: bool,
        /// Minimal mode: no hooks, no LSP, no CLAUDE.md discovery.
        pub bare: bool,
        /// Start with every customization disabled.
        pub safe_mode: bool,
        /// Bypass all permission checks. Sound only where the sandbox is
        /// the boundary.
        pub skip_permissions: bool,
        /// Stop the run at the first unmodelled event kind rather than
        /// carrying on past it.
        pub strict_protocol: bool,
    }

    /// One MCP server the child may reach.
    ///
    /// The transport is a discriminant with the other fields as its
    /// payload, the same flattening every enum takes at this boundary
    /// (ENG-11981): `stdio` reads `command`/`args`/`env` and ignores the
    /// rest, `http` and `sse` read `url`/`headers`.
    #[unibind::record]
    #[derive(Clone)]
    pub struct McpServer {
        /// The name. This server's tools reach the child as
        /// `mcp__<name>__<tool>`.
        pub name: String,
        /// `stdio`, `http`, or `sse`.
        pub transport: String,
        /// stdio: the executable to run.
        pub command: String,
        /// stdio: its arguments.
        pub args: Vec<String>,
        /// stdio: environment entries for it. A value may use the CLI's
        /// own `${VAR:-}` expansion.
        pub env: HashMap<String, String>,
        /// http and sse: where to reach it.
        pub url: String,
        /// http and sse: extra request headers.
        pub headers: HashMap<String, String>,
    }

    /// One headless Claude Code run.
    ///
    /// Start from `default_config/0` and update the fields that matter:
    /// every key is enforced, so a literal has to name all of them.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Config {
        /// The prompt. Must be `nil` when `input_format` is
        /// `"stream-json"`, where the prompt is a stdin line.
        pub prompt: Option<String>,
        /// An alias (`opus`, `sonnet`, `fable`) or a full model name.
        pub model: Option<String>,
        /// Models to fall back to when the primary is overloaded.
        pub fallback_models: Vec<String>,
        /// One of `permission_modes/0`; `nil` leaves it to the child's own
        /// settings.
        pub permission_mode: Option<String>,
        /// One of `low`, `medium`, `high`, `xhigh`, `max`.
        pub effort: Option<String>,
        /// The tool allowlist. `nil` passes no flag; `[]` passes an empty
        /// list, which is not the same thing.
        pub allowed_tools: Option<Vec<String>>,
        /// The tool denylist, same `nil`/`[]` distinction.
        pub disallowed_tools: Option<Vec<String>>,
        /// Restrict the built-in tool set. `[]` disables all built-in
        /// tools; MCP tools are unaffected, so pair it with
        /// `mcp_policy: "none"` for a genuinely tool-less run.
        pub tools: Option<Vec<String>>,
        /// Directories outside the working directory that tools may touch.
        pub add_dirs: Vec<String>,
        /// `text`, `json`, or `stream-json`. Only `stream-json` can be
        /// parsed into events.
        pub output_format: String,
        /// `text` or `stream-json`.
        pub input_format: String,
        /// `new`, `resume`, or `continue`.
        pub session_mode: String,
        /// With `session_mode: "new"` the id to mint (a UUID); with
        /// `"resume"` the id to resume. Ignored by `"continue"`.
        pub session_id: Option<String>,
        /// `inherit` (whatever the environment configures), `none` (no MCP
        /// servers at all, the kernel's depth-1 lockdown), or `only`
        /// (exactly `mcp_servers` plus `mcp_configs`).
        ///
        /// `only` is exact against the real CLI and not against a wrapper
        /// script: `--strict-mcp-config` ignores other configuration
        /// *sources*, but repeated `--mcp-config` flags layer, so a wrapper
        /// that passes its own adds servers this cannot subtract. Fix that
        /// at wrapper build time (`claude-code.override { mcpServers = ...; }`).
        pub mcp_policy: String,
        /// Servers to generate a config for, under `mcp_policy: "only"`.
        /// Rendered as one `--mcp-config` payload.
        pub mcp_servers: Vec<McpServer>,
        /// JSON strings or paths to JSON files, passed through as they are
        /// under `mcp_policy: "only"`.
        pub mcp_configs: Vec<String>,
        /// Replace the default system prompt.
        pub system_prompt: Option<String>,
        /// Add to the default system prompt.
        pub append_system_prompt: Option<String>,
        /// A settings file path or a JSON string.
        pub settings: Option<String>,
        /// Which settings layers to load: `user`, `project`, `local`.
        pub setting_sources: Option<Vec<String>>,
        /// Stop the run past this spend.
        pub max_budget_usd: Option<f64>,
        /// Feature toggles.
        pub features: Features,
        /// The working directory to spawn in. `nil` inherits the BEAM's,
        /// which is a process-global no caller controls.
        pub cwd: Option<String>,
        /// The executable. `nil` resolves `claude` on `PATH`.
        pub bin: Option<String>,
        /// Arguments for `bin` itself, rendered before `-p`, for the case
        /// where it is a launcher that execs `claude` (the kernel's loom
        /// runner passes a remote-exec wrapper the VM name and working
        /// directory).
        pub launcher_args: Vec<String>,
        /// Environment entries overlaid on the BEAM's own.
        pub env: HashMap<String, String>,
    }

    /// What the `init` event announces.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Init {
        /// The session's id, and the handle a resume needs.
        pub session_id: String,
        /// The model actually selected, which is not always the one asked
        /// for.
        pub model: String,
        /// The permission mode in force. Worth reading back: a wrapper
        /// script can override what the config asked for.
        pub permission_mode: String,
        /// The working directory the child resolved.
        pub cwd: String,
        /// Every tool the child can call, built-in and MCP alike.
        pub tools: Vec<String>,
        /// The CLI's version.
        pub claude_code_version: String,
    }

    /// The terminal result of a turn.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Outcome {
        /// The session this run belongs to.
        pub session_id: String,
        /// `success`, or one of the CLI's failure subtypes.
        pub subtype: String,
        /// Whether the run failed. Independent of the exit status.
        pub is_error: bool,
        /// The final assistant text.
        pub text: String,
        /// How many assistant turns the run took.
        pub num_turns: u64,
        /// Wall time, the CLI's own startup included.
        pub duration_ms: u64,
        /// Time spent in API calls.
        pub duration_api_ms: u64,
        /// What the run cost.
        pub total_cost_usd: f64,
    }

    /// One content block of an assistant or user turn.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Block {
        /// `text`, `thinking`, `tool_use`, `tool_result`, or `other`.
        pub kind: String,
        /// The prose, for `text` blocks. Thinking text is deliberately not
        /// carried across the boundary.
        pub text: String,
        /// The tool's name, for `tool_use`.
        pub name: String,
        /// The call's id (`tool_use`) or the id being answered
        /// (`tool_result`).
        pub id: String,
        /// The call's arguments as JSON, for `tool_use`.
        pub input: String,
        /// Whether a `tool_result` reported failure.
        pub is_error: bool,
    }

    /// One line of `--output-format stream-json`.
    ///
    /// `kind` is the flattened variant: `init`, `system`, `assistant`,
    /// `user`, `partial`, `rate_limit`, `result`, `unrecognized`,
    /// `not_json`, `exited`. The last event of every stream is `exited`, so
    /// a consumer never has to guess whether the run finished.
    ///
    /// `raw` is the line as it arrived, so anything this record does not
    /// model is still one `JSON.decode!/1` away.
    #[unibind::record]
    #[derive(Clone)]
    pub struct Event {
        /// The flattened variant.
        pub kind: String,
        /// The `system` subtype, or the unrecognised `type`; empty
        /// otherwise.
        pub subtype: String,
        /// The session, for the events that name one.
        pub session_id: Option<String>,
        /// The content blocks of an `assistant` or `user` turn.
        pub blocks: Vec<Block>,
        /// Present on `init`.
        pub init: Option<Init>,
        /// Present on `result`.
        pub outcome: Option<Outcome>,
        /// Present on `exited`: the status, or `nil` when a signal ended
        /// the child.
        pub exit_code: Option<i64>,
        /// Present on `exited`: what the child wrote to stderr.
        pub stderr: String,
        /// The line as it arrived.
        pub raw: String,
    }

    /// A config with every field at its default: `stream-json` output,
    /// `text` input, a new session, inherited MCP, and the house feature
    /// defaults.
    ///
    /// Elixir enforces every record key, so this is how a caller writes a
    /// config without naming two dozen fields.
    pub fn default_config() -> Config {
        from_core(core::Config::default())
    }

    /// A config whose child's entire tool surface is `server`.
    ///
    /// One call for the shape the star topology wants: `--tools ""` empties
    /// the built-in set, a generated `--mcp-config` names the one server,
    /// `--strict-mcp-config` stops another configuration source adding
    /// more, and `builtin_agents` goes off so the CLI's own subagent types
    /// are not a second spawn path.
    ///
    /// Set `prompt` (and anything else) on the result:
    ///
    /// ```elixir
    /// server = %ClaudeLaunch.McpServer{
    ///   name: "index", transport: "stdio", command: "ix-mcp-ex",
    ///   args: [], env: %{}, url: "", headers: %{}
    /// }
    /// {:ok, config} = ClaudeLaunch.kernel_only(server)
    /// {:ok, outcome} = ClaudeLaunch.run(%{config | prompt: "hi"})
    /// ```
    ///
    /// This is a tool-surface lever, not a depth limit. A child holding the
    /// kernel holds `Agents.spawn`, so depth-1 stops being structural and
    /// rests on the `IX_AGENT_CHILD` runtime guard in
    /// `packages/mcp-ex/lib/ix_mcp/agents.ex`; read that module before
    /// relying on the shape. The "exactly one server" half is also exact
    /// only against an unwrapped `claude` (see `mcp_policy`).
    ///
    /// # Errors
    ///
    /// `:config` when the server's transport is not one of `stdio`, `http`,
    /// `sse`, or when the fields that transport needs are empty.
    pub fn kernel_only(server: McpServer) -> Result<Config, ClaudeError> {
        Ok(from_core(
            core::Config::default().kernel_only(to_core_server(server)?),
        ))
    }

    /// The `permission_mode` values the CLI accepts.
    ///
    /// Exported because the flattening to `String` (ENG-11981) leaves no
    /// other way for a caller to discover the set.
    pub fn permission_modes() -> Vec<String> {
        core::PermissionMode::all()
            .iter()
            .map(|mode| mode.as_str().to_owned())
            .collect()
    }

    /// The child's argv, executable at index 0.
    ///
    /// For callers that own their own spawn. The kernel's subagent runner
    /// drives `claude` through an Erlang `Port` because it needs the stdin
    /// injection channel, and this is all it wants from the launcher.
    ///
    /// # Errors
    ///
    /// `:config` when the combination is one the CLI rejects.
    pub fn argv(config: Config) -> Result<Vec<String>, ClaudeError> {
        to_core(config)?.argv().map_err(convert)
    }

    /// The environment entries to overlay on the BEAM's own.
    ///
    /// # Errors
    ///
    /// `:config` when the combination is one the CLI rejects.
    pub fn env(config: Config) -> Result<HashMap<String, String>, ClaudeError> {
        let core = to_core(config)?;
        core.validate().map_err(convert)?;
        Ok(core.env().into_iter().collect())
    }

    /// Launch `claude` and stream its events.
    ///
    /// Demand-driven: nothing is produced without a granted credit. The
    /// producer is tied to the calling process, so a caller that exits
    /// mid-run kills the child rather than leaving a session that keeps
    /// billing.
    ///
    /// The last event is always `kind: "exited"`.
    ///
    /// Stopping the enumeration early (`Enum.take/2`, a `break`) stops the
    /// events but does not stop the child: on this backend a producer is
    /// only aborted when the process that started the stream exits
    /// (ENG-11989). Stream from a process that ends, or drive the
    /// demand-driven `events_stream/1` form from one you can stop.
    ///
    /// # Errors
    ///
    /// `:config` for a rejected combination, `:spawn` when the child could
    /// not be started.
    pub fn events(config: Config) -> Result<UniStream<Event>, ClaudeError> {
        let core = to_core(config)?;
        // tokio's process spawn registers the child with the reactor, so it
        // has to happen inside the shared runtime's context; a stream
        // function is a plain sync NIF and is not in one by default.
        let runtime = unibind_ex_runtime::runtime();
        let _guard = runtime.enter();
        ensure_reapable();
        let stream = core::event_stream(&core).map_err(convert)?;
        Ok(UniStream::new(futures::StreamExt::map(stream, from_event)))
    }

    /// Run to completion and return the terminal result.
    ///
    /// # Errors
    ///
    /// `:config`, `:spawn`, `:protocol` when the child emitted an event
    /// kind the launcher does not model and `strict_protocol` is on, and
    /// `:exited` when the child ended without a result.
    pub async fn run(config: Config) -> Result<Outcome, ClaudeError> {
        let core = to_core(config)?;
        ensure_reapable();
        core::run(&core).await.map_err(convert).map(from_outcome)
    }

    /// Restore `SIGCHLD` before the first spawn; the module docs say why
    /// this is not optional and why darwin will not prove it.
    fn ensure_reapable() {
        unibind_ex_runtime::ensure_sigchld_default();
    }

    fn convert(error: core::Error) -> ClaudeError {
        let message = error.to_string();
        match error {
            core::Error::Config { .. } => ClaudeError::Config { message },
            core::Error::Spawn { .. } => ClaudeError::Spawn { message },
            core::Error::Protocol { .. } => ClaudeError::Protocol { message },
            core::Error::Exited { .. } => ClaudeError::Exited { message },
        }
    }

    /// Parse a closed-set field, mapping the core parse error onto the
    /// boundary's `:config` variant. The accepted values ride in the
    /// message, which is the only way a caller in a language without enums
    /// can discover them (ENG-11981).
    fn parse_optional<T, F>(text: Option<&str>, parse: F) -> Result<Option<T>, ClaudeError>
    where
        F: Fn(&str) -> Result<T, core::Error>,
    {
        text.map(parse).transpose().map_err(convert)
    }

    fn parse_session(mode: &str, id: Option<String>) -> Result<core::SessionMode, ClaudeError> {
        match (mode, id) {
            ("new", None) => Ok(core::SessionMode::New),
            ("new", Some(id)) => Ok(core::SessionMode::NewWithId(id)),
            ("resume", Some(id)) => Ok(core::SessionMode::Resume(id)),
            ("resume", None) => Err(ClaudeError::Config {
                message: "session_mode `resume` needs a session_id".to_owned(),
            }),
            ("continue", _) => Ok(core::SessionMode::Continue),
            (other, _) => Err(ClaudeError::Config {
                message: format!(
                    "unknown session_mode `{other}`; expected one of new, resume, continue"
                ),
            }),
        }
    }

    fn parse_mcp(
        policy: &str,
        servers: Vec<McpServer>,
        configs: Vec<String>,
    ) -> Result<core::McpPolicy, ClaudeError> {
        match policy {
            "inherit" => Ok(core::McpPolicy::Inherit),
            "none" => Ok(core::McpPolicy::None),
            "only" => Ok(core::McpPolicy::Only {
                servers: servers
                    .into_iter()
                    .map(to_core_server)
                    .collect::<Result<Vec<_>, _>>()?,
                configs,
            }),
            other => Err(ClaudeError::Config {
                message: format!(
                    "unknown mcp_policy `{other}`; expected one of inherit, none, only"
                ),
            }),
        }
    }

    /// Refuse a transport whose payload fields are empty rather than
    /// generating a config the CLI will reject several seconds later on the
    /// child's stderr.
    fn require(value: String, field: &str, transport: &str) -> Result<String, ClaudeError> {
        if value.is_empty() {
            return Err(ClaudeError::Config {
                message: format!("an mcp server with transport `{transport}` needs a `{field}`"),
            });
        }
        Ok(value)
    }

    fn to_core_server(server: McpServer) -> Result<core::McpServer, ClaudeError> {
        let McpServer {
            name,
            transport,
            command,
            args,
            env,
            url,
            headers,
        } = server;
        let headers = || headers.clone().into_iter().collect();
        let transport = match transport.as_str() {
            "stdio" => core::McpTransport::Stdio {
                command: require(command, "command", "stdio")?,
                args,
                env: env.into_iter().collect(),
            },
            "http" => core::McpTransport::Http {
                url: require(url, "url", "http")?,
                headers: headers(),
            },
            "sse" => core::McpTransport::Sse {
                url: require(url, "url", "sse")?,
                headers: headers(),
            },
            other => {
                return Err(ClaudeError::Config {
                    message: format!(
                        "unknown mcp server transport `{other}`; expected one of stdio, http, sse"
                    ),
                });
            }
        };
        Ok(core::McpServer { name, transport })
    }

    fn from_core_server(server: core::McpServer) -> McpServer {
        let mut flat = McpServer {
            name: server.name,
            transport: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            headers: HashMap::new(),
        };
        match server.transport {
            core::McpTransport::Stdio { command, args, env } => {
                flat.transport = "stdio".to_owned();
                flat.command = command;
                flat.args = args;
                flat.env = env.into_iter().collect();
            }
            core::McpTransport::Http { url, headers } => {
                flat.transport = "http".to_owned();
                flat.url = url;
                flat.headers = headers.into_iter().collect();
            }
            core::McpTransport::Sse { url, headers } => {
                flat.transport = "sse".to_owned();
                flat.url = url;
                flat.headers = headers.into_iter().collect();
            }
        }
        flat
    }

    fn parse_setting_sources(
        sources: Option<Vec<String>>,
    ) -> Result<Option<Vec<core::SettingSource>>, ClaudeError> {
        sources
            .map(|sources| {
                sources
                    .iter()
                    .map(|source| core::SettingSource::parse(source))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(convert)
            })
            .transpose()
    }

    mirror!(to_core_features: Features => core::Features {
        partial_messages,
        hook_events,
        subagent_text,
        replay_user_messages,
        fork_session,
        session_persistence,
        slash_commands,
        builtin_agents,
        bare,
        safe_mode,
        skip_permissions,
        strict_protocol,
    });

    mirror!(from_core_features: core::Features => Features {
        partial_messages,
        hook_events,
        subagent_text,
        replay_user_messages,
        fork_session,
        session_persistence,
        slash_commands,
        builtin_agents,
        bare,
        safe_mode,
        skip_permissions,
        strict_protocol,
    });

    fn to_core(config: Config) -> Result<core::Config, ClaudeError> {
        let Config {
            prompt,
            model,
            fallback_models,
            permission_mode,
            effort,
            allowed_tools,
            disallowed_tools,
            tools,
            add_dirs,
            output_format,
            input_format,
            session_mode,
            session_id,
            mcp_policy,
            mcp_servers,
            mcp_configs,
            system_prompt,
            append_system_prompt,
            settings,
            setting_sources,
            max_budget_usd,
            features,
            cwd,
            bin,
            launcher_args,
            env,
        } = config;
        let permission_mode =
            parse_optional(permission_mode.as_deref(), core::PermissionMode::parse)?;
        let effort = parse_optional(effort.as_deref(), core::Effort::parse)?;
        Ok(core::Config {
            prompt,
            model,
            fallback_models,
            permission_mode,
            effort,
            allowed_tools,
            disallowed_tools,
            tools,
            add_dirs: add_dirs.into_iter().map(PathBuf::from).collect(),
            output_format: core::OutputFormat::parse(&output_format).map_err(convert)?,
            input_format: core::InputFormat::parse(&input_format).map_err(convert)?,
            session: parse_session(&session_mode, session_id)?,
            mcp: parse_mcp(&mcp_policy, mcp_servers, mcp_configs)?,
            system_prompt,
            append_system_prompt,
            settings,
            setting_sources: parse_setting_sources(setting_sources)?,
            max_budget_usd,
            features: to_core_features(features),
            cwd: cwd.map(PathBuf::from),
            bin: bin.map(PathBuf::from),
            launcher_args,
            env: env.into_iter().collect(),
        })
    }

    fn from_core(config: core::Config) -> Config {
        let (session_mode, session_id) = match config.session {
            core::SessionMode::New => ("new", None),
            core::SessionMode::NewWithId(id) => ("new", Some(id)),
            core::SessionMode::Resume(id) => ("resume", Some(id)),
            core::SessionMode::Continue => ("continue", None),
        };
        let (mcp_policy, mcp_servers, mcp_configs) = match config.mcp {
            core::McpPolicy::Inherit => ("inherit", Vec::new(), Vec::new()),
            core::McpPolicy::None => ("none", Vec::new(), Vec::new()),
            core::McpPolicy::Only { servers, configs } => (
                "only",
                servers.into_iter().map(from_core_server).collect(),
                configs,
            ),
        };
        Config {
            prompt: config.prompt,
            model: config.model,
            fallback_models: config.fallback_models,
            permission_mode: config.permission_mode.map(|mode| mode.as_str().to_owned()),
            effort: config.effort.map(|effort| effort.as_str().to_owned()),
            allowed_tools: config.allowed_tools,
            disallowed_tools: config.disallowed_tools,
            tools: config.tools,
            add_dirs: config
                .add_dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect(),
            output_format: config.output_format.as_str().to_owned(),
            input_format: config.input_format.as_str().to_owned(),
            session_mode: session_mode.to_owned(),
            session_id,
            mcp_policy: mcp_policy.to_owned(),
            mcp_servers,
            mcp_configs,
            system_prompt: config.system_prompt,
            append_system_prompt: config.append_system_prompt,
            settings: config.settings,
            setting_sources: config.setting_sources.map(|sources| {
                sources
                    .iter()
                    .map(|source| source.as_str().to_owned())
                    .collect()
            }),
            max_budget_usd: config.max_budget_usd,
            features: from_core_features(config.features),
            cwd: config.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            bin: config.bin.as_ref().map(|bin| bin.display().to_string()),
            launcher_args: config.launcher_args,
            env: config.env.into_iter().collect(),
        }
    }

    mirror!(from_outcome: core::Outcome => Outcome {
        session_id,
        subtype,
        is_error,
        text,
        num_turns,
        duration_ms,
        duration_api_ms,
        total_cost_usd,
    });

    fn from_block(block: core::Block) -> Block {
        let mut flat = Block {
            kind: String::new(),
            text: String::new(),
            name: String::new(),
            id: String::new(),
            input: String::new(),
            is_error: false,
        };
        match block {
            core::Block::Text(text) => {
                flat.kind = "text".to_owned();
                flat.text = text;
            }
            core::Block::Thinking => flat.kind = "thinking".to_owned(),
            core::Block::ToolUse { name, id, input } => {
                flat.kind = "tool_use".to_owned();
                flat.name = name;
                flat.id = id;
                flat.input = input;
            }
            core::Block::ToolResult {
                tool_use_id,
                is_error,
            } => {
                flat.kind = "tool_result".to_owned();
                flat.id = tool_use_id;
                flat.is_error = is_error;
            }
            core::Block::Other { kind } => {
                flat.kind = "other".to_owned();
                flat.name = kind;
            }
        }
        flat
    }

    mirror!(from_init: core::Init => Init {
        session_id,
        model,
        permission_mode,
        cwd,
        tools,
        claude_code_version,
    });

    fn from_event(event: core::Event) -> Event {
        let mut flat = Event {
            kind: event.kind().to_owned(),
            subtype: String::new(),
            session_id: event.session_id().map(ToOwned::to_owned),
            blocks: Vec::new(),
            init: None,
            outcome: None,
            exit_code: None,
            stderr: String::new(),
            raw: String::new(),
        };
        match event {
            core::Event::Init(init) => {
                flat.subtype = "init".to_owned();
                flat.raw = init.raw.clone();
                flat.init = Some(from_init(init));
            }
            core::Event::System { subtype, raw } => {
                flat.subtype = subtype;
                flat.raw = raw;
            }
            core::Event::Assistant(message) | core::Event::User(message) => {
                flat.blocks = message.blocks.into_iter().map(from_block).collect();
                flat.raw = message.raw;
            }
            core::Event::Partial { raw } | core::Event::RateLimit { raw } => flat.raw = raw,
            core::Event::Result(outcome) => {
                flat.subtype = outcome.subtype.clone();
                flat.raw = outcome.raw.clone();
                flat.outcome = Some(from_outcome(outcome));
            }
            core::Event::Unrecognized { kind, raw } => {
                flat.subtype = kind;
                flat.raw = raw;
            }
            core::Event::NotJson { line } => flat.raw = line,
            core::Event::Exited { code, stderr } => {
                flat.exit_code = code.map(i64::from);
                flat.stderr = stderr;
            }
        }
        flat
    }
}
