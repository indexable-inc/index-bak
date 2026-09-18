//! What to launch: the typed replacement for a hand-assembled argv.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// Everything the boundary refuses to carry forward.
///
/// A child that merely exits nonzero is not one of these on its own: the run
/// reports that as data (`Event::Exited`). These are the four ways a launch
/// cannot proceed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The [`Config`] asks for a combination the CLI rejects, caught here so
    /// the failure names the field instead of arriving as a usage message on
    /// the child's stderr.
    Config {
        /// What is wrong and what to set instead.
        message: String,
    },
    /// The child process could not be started (binary missing, cwd gone).
    Spawn {
        /// The underlying OS failure.
        message: String,
    },
    /// The child's output broke the stream-json contract this crate mirrors.
    Protocol {
        /// What arrived and what was expected.
        message: String,
    },
    /// The child ended without a terminal `result` event.
    Exited {
        /// The exit status and whatever the child wrote to stderr.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config { message }
            | Self::Spawn { message }
            | Self::Protocol { message }
            | Self::Exited { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// Declare a closed set of CLI spellings.
///
/// Five of [`Config`]'s fields are the same shape: a fixed set of values the
/// CLI accepts, each with exactly one spelling on the command line. Writing
/// the enum, the `as_str`, the `all` and the `parse` out five times is five
/// chances for a spelling to drift away from the variant it belongs to, so
/// the shape is declared once and the five uses give it their values.
///
/// `label` is how the type is named to a caller in the parse error, which is
/// the only way a caller in a language without enums can discover the set
/// (see the module docs of `claude-launch-ex` and ENG-11981).
macro_rules! cli_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $label:literal {
            $($(#[$variant_meta:meta])* $variant:ident => $spelling:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            /// The spelling the CLI expects.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $spelling,)+
                }
            }

            /// Every value, in the order `claude --help` lists them.
            #[must_use]
            pub const fn all() -> &'static [Self] {
                &[$(Self::$variant,)+]
            }

            /// Parse a CLI spelling.
            ///
            /// # Errors
            ///
            /// [`Error::Config`] when `text` is not one of
            #[doc = concat!("[`", stringify!($name), "::all`]; the message names every")]
            /// accepted value.
            pub fn parse(text: &str) -> Result<Self, Error> {
                Self::all()
                    .iter()
                    .copied()
                    .find(|value| value.as_str() == text)
                    .ok_or_else(|| Error::Config {
                        message: format!(
                            "unknown {} `{text}`; expected one of {}",
                            $label,
                            Self::all()
                                .iter()
                                .map(|value| value.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    })
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

cli_enum! {
    /// The `--permission-mode` values Claude Code accepts.
    ///
    /// A closed set, so it is an enum rather than the bare string every
    /// current caller passes. The CLI validates it too, but only after the
    /// process has started and only as a usage message on stderr.
    PermissionMode, "permission mode" {
        /// Apply edits without asking; still prompts for other tools.
        AcceptEdits => "acceptEdits",
        /// Let the classifier decide per tool call.
        Auto => "auto",
        /// Ask for nothing. Only sound inside a sandbox that is itself the
        /// boundary (a disposable VM), never on a developer machine.
        BypassPermissions => "bypassPermissions",
        /// Prompt for everything. A headless run under this mode stalls on
        /// the first tool call, which is the point when the run is meant to
        /// be read-only.
        Manual => "manual",
        /// Run without prompting but without the accept-edits shortcut.
        DontAsk => "dontAsk",
        /// Plan only: no edits, no commands.
        Plan => "plan",
    }
}

cli_enum! {
    /// The `--output-format` values.
    OutputFormat, "output format" {
        /// The final text and nothing else.
        Text => "text",
        /// One JSON object for the whole run.
        Json => "json",
        /// One JSON object per line, as it happens. The only format this
        /// crate can parse into [`crate::Event`] values.
        StreamJson => "stream-json",
    }
}

cli_enum! {
    /// The `--input-format` values.
    InputFormat, "input format" {
        /// The prompt rides argv and stdin is not read.
        Text => "text",
        /// The prompt (and every later turn) arrives as JSON lines on stdin.
        /// This is the message-injection channel the kernel's runner uses.
        StreamJson => "stream-json",
    }
}

cli_enum! {
    /// The `--effort` levels.
    Effort, "effort level" {
        /// Least thinking.
        Low => "low",
        /// The default.
        Medium => "medium",
        /// More thinking.
        High => "high",
        /// More still.
        Xhigh => "xhigh",
        /// The most the model will do.
        Max => "max",
    }
}

cli_enum! {
    /// The `--setting-sources` values: which settings layers to load.
    SettingSource, "setting source" {
        /// `~/.claude/settings.json`.
        User => "user",
        /// The repository's checked-in settings.
        Project => "project",
        /// The repository's gitignored local settings.
        Local => "local",
    }
}

/// Which conversation the run belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMode {
    /// A fresh session; the CLI picks the id and announces it in the `init`
    /// event.
    New,
    /// A fresh session under an id the caller picked (`--session-id`), so the
    /// caller can address the session before the child has said anything.
    /// Must be a UUID; the CLI refuses anything else.
    NewWithId(String),
    /// `--resume <id>`: continue a session by id.
    Resume(String),
    /// `--continue`: continue the most recent session in the working
    /// directory.
    Continue,
}

/// How a child reaches one MCP server.
///
/// The three shapes `--mcp-config` accepts, keyed by the `type` field of
/// each entry under `mcpServers`. Read off the config the wrapper generates
/// (`claude-code-mcp-config.json`), which uses two of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A local process speaking MCP over its stdio.
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Environment entries for it. A value may use the CLI's own
        /// `${VAR:-}` expansion, which is how the wrapper forwards secrets
        /// without baking them into a store path.
        env: BTreeMap<String, String>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Where to reach it.
        url: String,
        /// Extra request headers.
        headers: BTreeMap<String, String>,
    },
    /// A server-sent-events endpoint.
    Sse {
        /// Where to reach it.
        url: String,
        /// Extra request headers.
        headers: BTreeMap<String, String>,
    },
}

/// One MCP server, named as the child will see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The name. The child's tools from this server appear as
    /// `mcp__<name>__<tool>`, which is also how a tool policy would have to
    /// spell them.
    pub name: String,
    /// How to reach it.
    pub transport: McpTransport,
}

impl McpServer {
    /// A stdio server: a local process, no arguments, no environment.
    #[must_use]
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        }
    }

    /// This server's entry under `mcpServers`.
    ///
    /// Built through a `BTreeMap` rather than `serde_json::json!`, which
    /// keeps the order the literal was written in. That order is only
    /// *stable* when `serde_json`'s `preserve_order` feature is off, and a
    /// feature is a whole-workspace property: cargo unifies it across every
    /// member, so whether this crate's JSON comes out sorted or
    /// literal-ordered depends on what some unrelated crate turned on. A
    /// `BTreeMap` is sorted going in, so both spellings of `serde_json`
    /// emit the same bytes. (Caught by CI: the same assertion passed on a
    /// developer checkout building this crate alone and failed in the nix
    /// sandbox building it inside the workspace.)
    fn entry(&self) -> serde_json::Value {
        let mut fields: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
        match &self.transport {
            McpTransport::Stdio { command, args, env } => {
                fields.insert("type", "stdio".into());
                fields.insert("command", command.as_str().into());
                fields.insert("args", args.clone().into());
                fields.insert("env", serde_json::to_value(env).unwrap_or_default());
            }
            McpTransport::Http { url, headers } => {
                endpoint(&mut fields, "http", url, headers);
            }
            McpTransport::Sse { url, headers } => {
                endpoint(&mut fields, "sse", url, headers);
            }
        }
        serde_json::to_value(fields).unwrap_or_default()
    }
}

/// The fields an http or sse entry carries; they differ only in the tag.
fn endpoint(
    fields: &mut BTreeMap<&'static str, serde_json::Value>,
    kind: &'static str,
    url: &str,
    headers: &BTreeMap<String, String>,
) {
    fields.insert("type", kind.into());
    fields.insert("url", url.into());
    fields.insert("headers", serde_json::to_value(headers).unwrap_or_default());
}

/// Render servers as one `--mcp-config` payload.
///
/// Byte-stable, key order included: the argv ends up in logs, in a Port
/// spec, and in test assertions, and a value that reorders between builds
/// is a diff nobody can read. See [`McpServer::entry`] for why that takes
/// care rather than falling out.
pub(crate) fn mcp_config_json(servers: &[McpServer]) -> String {
    let map: BTreeMap<&str, serde_json::Value> = servers
        .iter()
        .map(|server| (server.name.as_str(), server.entry()))
        .collect();
    let mut root: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    root.insert("mcpServers", serde_json::to_value(map).unwrap_or_default());
    serde_json::to_value(root).unwrap_or_default().to_string()
}

/// Which MCP servers the child may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPolicy {
    /// Whatever the environment configures. A child launched this way
    /// inherits the parent's kernel and everything below it.
    Inherit,
    /// No MCP servers at all: `--strict-mcp-config` with an empty server
    /// map. This is the kernel's depth-1 lockdown, named rather than
    /// spelled out as a JSON literal at each call site.
    None,
    /// Exactly these, with `--strict-mcp-config` so no other configuration
    /// *source* is read.
    ///
    /// `servers` render as one generated `--mcp-config` payload; `configs`
    /// are passed through as they are (each a JSON string or a path to a
    /// JSON file), for a caller that already has one.
    ///
    /// # "Exactly these" has a boundary
    ///
    /// `--strict-mcp-config` ignores other configuration *sources* (a
    /// project `.mcp.json`, user settings). It does not subtract a server
    /// that another `--mcp-config` flag already added, and repeated flags
    /// layer. So this is exact against the real CLI and not against a
    /// wrapper that injects its own `--mcp-config`: index's wrapper does,
    /// and on 2026-08-02 a run under this policy still saw the wrapper's
    /// `index` and `exa` servers in its `init` event. The lever for that is
    /// at wrapper build time (`claude-code.override { mcpServers = ...; }`),
    /// not here.
    Only {
        /// Servers to generate a config for.
        servers: Vec<McpServer>,
        /// Configs to pass through untouched.
        configs: Vec<String>,
    },
}

/// Feature toggles, one named bool each.
///
/// The point of the struct is that "enable this, do not enable that" is a
/// field rather than a flag string a caller has to remember exists.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a bag of independent on/off features is exactly what this type is; \
              grouping them into sub-structs would only hide the flag each one is"
)]
pub struct Features {
    /// Emit partial message chunks as they arrive
    /// (`--include-partial-messages`). Requires stream-json output.
    pub partial_messages: bool,
    /// Emit hook lifecycle events (`--include-hook-events`). Requires
    /// stream-json output.
    pub hook_events: bool,
    /// Forward subagent text and thinking as messages
    /// (`--forward-subagent-text`). Requires stream-json output.
    pub subagent_text: bool,
    /// Echo stdin user messages back on stdout (`--replay-user-messages`),
    /// so an injector can acknowledge its own turns. Requires stream-json on
    /// both sides.
    pub replay_user_messages: bool,
    /// When resuming, mint a new session id instead of writing back into the
    /// old one (`--fork-session`). Requires a resuming [`SessionMode`].
    pub fork_session: bool,
    /// Persist the session to disk. `false` sets
    /// `--no-session-persistence`, which also makes the run unresumable.
    pub session_persistence: bool,
    /// Load skills. `false` sets `--disable-slash-commands`.
    pub slash_commands: bool,
    /// Let the child spawn built-in subagent types. `false` sets
    /// `CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS=1` in the child's
    /// environment: it is the env half of the kernel's depth-1 lockdown, and
    /// it is a feature here rather than an env entry because a caller
    /// thinking about "may this child spawn children" should not have to
    /// know the variable's name.
    pub builtin_agents: bool,
    /// Minimal mode (`--bare`): no hooks, no LSP, no CLAUDE.md discovery, no
    /// keychain. Auth becomes strictly `ANTHROPIC_API_KEY`.
    pub bare: bool,
    /// Start with every customization disabled (`--safe-mode`).
    pub safe_mode: bool,
    /// Bypass all permission checks (`--dangerously-skip-permissions`).
    /// Sound only where the sandbox is the boundary.
    pub skip_permissions: bool,
    /// Stop the event stream at the first event kind this crate does not
    /// model, instead of carrying on past it.
    ///
    /// On by default because the alternative is what the two Elixir
    /// dispatchers do today: a catch-all arm that drops the line. The
    /// unrecognized event is always emitted either way (see
    /// [`crate::Event::Unrecognized`]); this decides whether the run
    /// continues after it.
    pub strict_protocol: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            partial_messages: false,
            hook_events: false,
            subagent_text: false,
            replay_user_messages: false,
            fork_session: false,
            session_persistence: true,
            slash_commands: true,
            builtin_agents: true,
            bare: false,
            safe_mode: false,
            skip_permissions: false,
            strict_protocol: true,
        }
    }
}

/// One headless Claude Code run.
///
/// Build it with [`Config::print`] and the setters; [`Config::argv`] renders
/// the exact argv and [`Config::env`] the environment overlay, so a caller
/// that owns its own spawn (the kernel needs the stdin channel a `Port`
/// gives it) can adopt the typed config without giving up its process
/// handling.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The prompt, when it rides argv. Must be `None` under
    /// [`InputFormat::StreamJson`], where the prompt is a stdin line: a
    /// trailing positional argument there is swallowed by whichever variadic
    /// flag precedes it.
    pub prompt: Option<String>,
    /// `--model`: an alias (`opus`, `sonnet`, `fable`) or a full name. Free
    /// text on purpose, since the set moves faster than this crate.
    pub model: Option<String>,
    /// `--fallback-model`: models to try when the primary is overloaded.
    pub fallback_models: Vec<String>,
    /// `--permission-mode`. `None` leaves the choice to the child's own
    /// settings.
    pub permission_mode: Option<PermissionMode>,
    /// `--effort`.
    pub effort: Option<Effort>,
    /// `--allowedTools`. `None` passes no flag; `Some(vec![])` passes an
    /// empty list, which is not the same thing.
    ///
    /// This and [`Config::tools`] are different axes, and conflating them
    /// is the documented confusion (anthropics/claude-code#2077):
    /// `--allowedTools` decides what runs *without a permission prompt*,
    /// `--tools` decides what *exists*. So this gates approval, not
    /// availability: under [`Features::skip_permissions`] there is no
    /// permission check left for it to narrow and an empty allowlist is
    /// inert, while an empty `tools` removes the built-ins outright.
    pub allowed_tools: Option<Vec<String>>,
    /// `--disallowedTools`, same `None`/empty distinction.
    pub disallowed_tools: Option<Vec<String>>,
    /// `--tools`: restrict the built-in set. `Some(vec![])` disables all
    /// built-in tools, and only those: MCP tools are a separate axis, so a
    /// genuinely tool-less run pairs it with [`McpPolicy::None`].
    pub tools: Option<Vec<String>>,
    /// `--add-dir`: directories outside the working directory that tools may
    /// touch.
    pub add_dirs: Vec<PathBuf>,
    /// `--output-format`.
    pub output_format: OutputFormat,
    /// `--input-format`.
    pub input_format: InputFormat,
    /// Which conversation this run belongs to.
    pub session: SessionMode,
    /// Which MCP servers the child may reach.
    pub mcp: McpPolicy,
    /// `--system-prompt`: replace the default system prompt.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt`: add to the default system prompt.
    pub append_system_prompt: Option<String>,
    /// `--settings`: a settings file path or a JSON string.
    pub settings: Option<String>,
    /// `--setting-sources`. `None` leaves the CLI's own default.
    pub setting_sources: Option<Vec<SettingSource>>,
    /// `--max-budget-usd`: stop the run past this spend.
    pub max_budget_usd: Option<f64>,
    /// Feature toggles.
    pub features: Features,
    /// The working directory to spawn in. `None` inherits the caller's,
    /// which on the BEAM is a process-global the caller does not control.
    pub cwd: Option<PathBuf>,
    /// The executable. `None` resolves `claude` on `PATH`.
    pub bin: Option<PathBuf>,
    /// Arguments for [`Config::bin`] itself, rendered before `-p`.
    ///
    /// For the case where the executable is a launcher that eventually
    /// execs `claude`: the kernel's loom runner spawns a remote-exec
    /// wrapper and passes it the VM name and working directory
    /// (`packages/mcp-ex/lib/ix_mcp/agents/loom_runner.ex`). They render
    /// before the prompt, which keeps the prompt ahead of every variadic
    /// flag.
    pub launcher_args: Vec<String>,
    /// Environment entries overlaid on the caller's, on top of whatever
    /// [`Features`] implies.
    pub env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prompt: None,
            model: None,
            fallback_models: Vec::new(),
            permission_mode: None,
            effort: None,
            allowed_tools: None,
            disallowed_tools: None,
            tools: None,
            add_dirs: Vec::new(),
            // stream-json rather than text: this crate exists to parse
            // events, and a Config that defaults to a format it cannot parse
            // would fail at the first `run`.
            output_format: OutputFormat::StreamJson,
            input_format: InputFormat::Text,
            session: SessionMode::New,
            mcp: McpPolicy::Inherit,
            system_prompt: None,
            append_system_prompt: None,
            settings: None,
            setting_sources: None,
            max_budget_usd: None,
            features: Features::default(),
            cwd: None,
            bin: None,
            launcher_args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

impl Config {
    /// A one-shot print run on `prompt`, streaming events.
    #[must_use]
    pub fn print(prompt: impl Into<String>) -> Self {
        Self {
            prompt: Some(prompt.into()),
            ..Self::default()
        }
    }

    /// A print run whose prompt and later turns arrive on stdin.
    #[must_use]
    pub fn streamed_input() -> Self {
        Self {
            input_format: InputFormat::StreamJson,
            ..Self::default()
        }
    }

    /// Set the model.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the permission mode.
    #[must_use]
    pub const fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }

    /// Set the tool allowlist. An empty iterator is an empty allowlist, not
    /// an absent one.
    #[must_use]
    pub fn allowed_tools<T: Into<String>>(mut self, tools: impl IntoIterator<Item = T>) -> Self {
        self.allowed_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Set the tool denylist.
    #[must_use]
    pub fn disallowed_tools<T: Into<String>>(mut self, tools: impl IntoIterator<Item = T>) -> Self {
        self.disallowed_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Set which MCP servers the child may reach.
    #[must_use]
    pub fn mcp(mut self, policy: McpPolicy) -> Self {
        self.mcp = policy;
        self
    }

    /// Set the conversation this run belongs to.
    #[must_use]
    pub fn session(mut self, session: SessionMode) -> Self {
        self.session = session;
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set the executable.
    #[must_use]
    pub fn bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.bin = Some(bin.into());
        self
    }

    /// Replace the feature toggles.
    #[must_use]
    pub const fn features(mut self, features: Features) -> Self {
        self.features = features;
        self
    }

    /// Shape the run so the child's entire tool surface is `server`.
    ///
    /// Composes the three flags that together mean "no built-in tools,
    /// exactly this MCP server":
    ///
    /// - `--tools ""` empties the built-in set (Bash, Edit, Task, the rest).
    /// - `--mcp-config <generated>` names the one server.
    /// - `--strict-mcp-config` stops the child reading a project
    ///   `.mcp.json` or the user's settings.
    ///
    /// It deliberately leaves [`Config::allowed_tools`] alone: that is the
    /// approval axis, not the existence axis (see its docs and
    /// anthropics/claude-code#2077).
    ///
    /// Verified against the real CLI on 2026-08-02: with `--tools ""` the
    /// `init` event listed no built-in tool and only `mcp__`-prefixed ones,
    /// so the CLI does express "no built-ins, MCP only".
    ///
    /// # Two boundaries worth knowing before relying on this
    ///
    /// The "exactly one server" half is exact only against an unwrapped
    /// `claude`; see [`McpPolicy::Only`] for why a wrapper's own
    /// `--mcp-config` layers on top rather than being replaced.
    ///
    /// And this is a tool-surface lever, not a depth limit. Handing a child
    /// the kernel hands it `Agents.spawn`, so the star topology stops being
    /// structural (a child with no MCP server cannot spawn anything) and
    /// rests on the `IX_AGENT_CHILD` runtime guard in
    /// `packages/mcp-ex/lib/ix_mcp/agents.ex`. Read that module before
    /// relying on the shape, and keep
    /// [`Features::builtin_agents`] off so the child cannot reach the
    /// CLI's own subagents either.
    #[must_use]
    pub fn kernel_only(mut self, server: McpServer) -> Self {
        self.tools = Some(Vec::new());
        self.mcp = McpPolicy::Only {
            servers: vec![server],
            configs: Vec::new(),
        };
        self.features.builtin_agents = false;
        self
    }
}
