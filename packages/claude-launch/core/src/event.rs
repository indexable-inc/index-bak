//! The stream-json line protocol, parsed.
//!
//! # This schema is a mirror, not a definition
//!
//! Claude Code owns the shape of `--output-format stream-json`; this crate
//! only reflects it. There is nothing to generate from: `--output-format
//! schema` is rejected by the CLI, and the installed package ships no type
//! declarations. So the variants below were read off `claude --help` and off
//! real runs of the version in [`SCHEMA_CHECKED`], and they will fall behind
//! the day a new event kind ships.
//!
//! The whole point of the [`Event::Unrecognized`] variant is that the day it
//! falls behind is visible. Both Elixir dispatchers this crate replaces end
//! their `case` with a catch-all that records an empty `:meta` event or an
//! `{:other, map}` nobody reads, so a new event kind arrives as silence.
//! Here it arrives as a variant every exhaustive `match` has to handle, and
//! with `Features::strict_protocol` (the default) it also ends the run,
//! because a launcher that quietly ignores half a protocol is worse than one
//! that stops.

use serde::Deserialize;

/// The Claude Code build this schema was last read off, and when.
///
/// Quoted in the error text of a strict-protocol failure so the report says
/// how stale the mirror is without anyone going to look.
pub const SCHEMA_CHECKED: &str = "Claude Code 2.1.220, read 2026-08-02";

/// The `system` subtype announcing a new session.
const INIT_SUBTYPE: &str = "init";

/// One line of `--output-format stream-json`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `{"type":"system","subtype":"init",...}`: the session exists and this
    /// is what it can do. The `session_id` here is the handle a later
    /// [`crate::SessionMode::Resume`] needs.
    Init(Init),
    /// Any other `system` event: hook lifecycle (`hook_started`,
    /// `hook_response`), compaction notices, and whatever else the CLI
    /// reports about itself.
    System {
        /// The `subtype` field.
        subtype: String,
        /// The line as it arrived.
        raw: String,
    },
    /// A turn from the model.
    Assistant(Message),
    /// A turn back to the model: tool results, and injected user messages
    /// under stream-json input.
    User(Message),
    /// `{"type":"stream_event",...}`: one incremental chunk of a message,
    /// emitted only under `Features::partial_messages`. Left unparsed
    /// because the payload is the raw Anthropic streaming API event, a
    /// second schema this crate does not own either.
    Partial {
        /// The line as it arrived.
        raw: String,
    },
    /// `{"type":"rate_limit_event",...}`: the account's current limit
    /// standing. Neither Elixir dispatcher in this tree knows this kind
    /// exists; both drop it.
    RateLimit {
        /// The line as it arrived.
        raw: String,
    },
    /// The terminal event of a turn.
    Result(Outcome),
    /// A `type` this crate does not model. Never dropped: see the module
    /// docs for why this is a variant rather than a catch-all arm.
    Unrecognized {
        /// The `type` field that was not recognised.
        kind: String,
        /// The line as it arrived.
        raw: String,
    },
    /// A line that was not JSON at all: CLI notices, guest boot noise, a
    /// wrapper's own chatter. Kept rather than discarded, because on a
    /// failing run it is usually the only evidence there is.
    NotJson {
        /// The line as it arrived.
        line: String,
    },
    /// The child ended. Always the last event of a stream, on every path,
    /// so "the stream stopped" is never ambiguous.
    Exited {
        /// The exit status, or `None` when a signal ended the process.
        code: Option<i32>,
        /// What the child wrote to stderr, tail-truncated.
        stderr: String,
    },
}

/// What `{"type":"system","subtype":"init"}` announces.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Init {
    /// The session's id, and the handle a resume needs.
    pub session_id: String,
    /// The model actually selected, which is not always the one asked for.
    pub model: String,
    /// The permission mode in force, under the CLI's own spelling. Worth
    /// reading back: a wrapper script can override what the config asked
    /// for.
    #[serde(rename = "permissionMode")]
    pub permission_mode: String,
    /// The working directory the child resolved.
    pub cwd: String,
    /// Every tool the child can call, built-in and MCP alike.
    pub tools: Vec<String>,
    /// The CLI's version, so a mismatch with [`SCHEMA_CHECKED`] is visible
    /// in a log.
    pub claude_code_version: String,
    /// The line as it arrived. `init` carries far more than this struct
    /// models (every skill, every plugin, every MCP server), so the caller
    /// that wants one of those still has it.
    #[serde(skip)]
    pub raw: String,
}

/// A turn, from either side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The content blocks, in order.
    pub blocks: Vec<Block>,
    /// The line as it arrived.
    pub raw: String,
}

/// One content block of a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Prose.
    Text(String),
    /// Extended thinking. The text is deliberately not carried: nothing in
    /// this tree acts on it, and a launcher that logs it by default leaks
    /// reasoning into every consumer's transcript.
    Thinking,
    /// The model called a tool.
    ToolUse {
        /// The tool's name.
        name: String,
        /// The call's id, which the matching result carries back.
        id: String,
        /// The arguments, re-serialised. A JSON string rather than a typed
        /// value because every tool has its own schema.
        input: String,
    },
    /// A tool answered.
    ToolResult {
        /// The id of the call being answered.
        tool_use_id: String,
        /// Whether the tool reported failure.
        is_error: bool,
    },
    /// A block kind this crate does not model, kept for the same reason as
    /// [`Event::Unrecognized`].
    Other {
        /// The block's `type` field.
        kind: String,
    },
}

/// The terminal `result` event.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Outcome {
    /// The session this run belongs to.
    pub session_id: String,
    /// `success`, or one of the CLI's failure subtypes.
    pub subtype: String,
    /// Whether the run failed. Independent of the exit status: a run can
    /// report an error and still exit zero.
    pub is_error: bool,
    /// The final assistant text.
    #[serde(rename = "result")]
    pub text: String,
    /// How many assistant turns the run took.
    pub num_turns: u64,
    /// Wall time, including the CLI's own startup.
    pub duration_ms: u64,
    /// Time spent in API calls.
    pub duration_api_ms: u64,
    /// What the run cost.
    pub total_cost_usd: f64,
    /// The line as it arrived, carrying the per-model usage breakdown and
    /// the permission denials this struct does not model.
    #[serde(skip)]
    pub raw: String,
}

impl Event {
    /// Parse one line.
    ///
    /// Total by construction: an unparsable line is [`Event::NotJson`] and
    /// an unmodelled `type` is [`Event::Unrecognized`]. A stream on the
    /// BEAM has no error leg (a failing stream function fails before the
    /// stream exists), so a parse that could fail would have to either
    /// panic inside the producer or drop the line, and dropping the line is
    /// the behaviour this crate exists to remove.
    #[must_use]
    pub fn parse(line: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return Self::NotJson {
                line: line.to_owned(),
            };
        };
        let raw = line.to_owned();
        let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
            return Self::Unrecognized {
                kind: String::new(),
                raw,
            };
        };
        match kind {
            "system" => {
                let subtype = string_at(&value, "subtype");
                if subtype == INIT_SUBTYPE {
                    return match serde_json::from_value::<Init>(value) {
                        Ok(mut init) => {
                            init.raw = raw;
                            Self::Init(init)
                        }
                        // A malformed `init` is not a kind this crate fails
                        // to know about, so it stays a `system` event with
                        // the line attached rather than tripping the
                        // strict-protocol stop.
                        Err(_) => Self::System { subtype, raw },
                    };
                }
                Self::System { subtype, raw }
            }
            "assistant" => Self::Assistant(message(&value, raw)),
            "user" => Self::User(message(&value, raw)),
            "stream_event" => Self::Partial { raw },
            "rate_limit_event" => Self::RateLimit { raw },
            "result" => match serde_json::from_value::<Outcome>(value) {
                Ok(mut outcome) => {
                    outcome.raw = raw;
                    Self::Result(outcome)
                }
                Err(error) => Self::Unrecognized {
                    kind: format!("result (undecodable: {error})"),
                    raw,
                },
            },
            other => Self::Unrecognized {
                kind: other.to_owned(),
                raw,
            },
        }
    }

    /// The flattened discriminant.
    ///
    /// unibind's IR has no plain enum (ENG-11981), so this is what the
    /// exported record carries in place of the variant. Keeping the mapping
    /// here rather than in the binding crate means every backend that ever
    /// exports this type agrees on the spelling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::System { .. } => "system",
            Self::Assistant(_) => "assistant",
            Self::User(_) => "user",
            Self::Partial { .. } => "partial",
            Self::RateLimit { .. } => "rate_limit",
            Self::Result(_) => "result",
            Self::Unrecognized { .. } => "unrecognized",
            Self::NotJson { .. } => "not_json",
            Self::Exited { .. } => "exited",
        }
    }

    /// The session id, for the events that carry one.
    #[must_use]
    pub const fn session_id(&self) -> Option<&str> {
        match self {
            Self::Init(init) => Some(init.session_id.as_str()),
            Self::Result(outcome) => Some(outcome.session_id.as_str()),
            _ => None,
        }
    }
}

/// The `message.content` blocks of an assistant or user event.
fn message(value: &serde_json::Value, raw: String) -> Message {
    let blocks = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
        .map(|blocks| blocks.iter().map(block).collect())
        .unwrap_or_default();
    Message { blocks, raw }
}

/// One content block.
fn block(value: &serde_json::Value) -> Block {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => Block::Text(string_at(value, "text")),
        Some("thinking") => Block::Thinking,
        Some("tool_use") => Block::ToolUse {
            name: string_at(value, "name"),
            id: string_at(value, "id"),
            input: value
                .get("input")
                .map(serde_json::Value::to_string)
                .unwrap_or_default(),
        },
        Some("tool_result") => Block::ToolResult {
            tool_use_id: string_at(value, "tool_use_id"),
            is_error: value
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        },
        other => Block::Other {
            kind: other.unwrap_or_default().to_owned(),
        },
    }
}

/// A string field, empty when absent or not a string.
fn string_at(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Block, Event};

    /// A real `--output-format stream-json` run of Claude Code 2.1.220,
    /// captured 2026-08-02, with two lines appended by hand: an event kind
    /// that does not exist, and a line that is not JSON. Real rather than
    /// hand-written because the fields this crate skips are as much a part
    /// of the contract as the ones it reads.
    const CAPTURE: &str = include_str!("../tests/fixtures/stream-json-2.1.220.jsonl");

    fn parsed() -> Vec<Event> {
        CAPTURE
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(Event::parse)
            .collect()
    }

    #[test]
    fn a_real_capture_parses_to_the_kinds_it_contains() {
        let kinds: Vec<&str> = parsed().iter().map(Event::kind).collect();
        assert_eq!(
            kinds,
            [
                "system",
                "system",
                "init",
                "assistant",
                "rate_limit",
                "result",
                "unrecognized",
                "not_json",
            ]
        );
    }

    #[test]
    fn init_carries_the_handle_a_resume_needs() {
        let Some(Event::Init(init)) = parsed().into_iter().find(|e| e.kind() == "init") else {
            panic!("the capture has an init event");
        };
        assert_eq!(init.session_id, "31366d10-54cd-4b3e-86d1-62254cb03394");
        assert_eq!(init.claude_code_version, "2.1.220");
        // Read back rather than assumed: this run was wrapped by a script
        // that adds --dangerously-skip-permissions, so the mode in force is
        // not the one the caller asked for.
        assert_eq!(init.permission_mode, "bypassPermissions");
        assert!(init.tools.contains(&"Bash".to_owned()));
        assert!(!init.raw.is_empty(), "the unmodelled half is still there");
    }

    #[test]
    fn an_assistant_turn_yields_its_text_block() {
        let Some(Event::Assistant(message)) =
            parsed().into_iter().find(|e| e.kind() == "assistant")
        else {
            panic!("the capture has an assistant event");
        };
        assert_eq!(
            message.blocks,
            [Block::Text("Hi! What can I help you with?".to_owned())]
        );
    }

    #[test]
    fn the_result_carries_the_final_text_and_the_bill() {
        let Some(Event::Result(outcome)) = parsed().into_iter().find(|e| e.kind() == "result")
        else {
            panic!("the capture has a result event");
        };
        assert!(!outcome.is_error);
        assert_eq!(outcome.subtype, "success");
        assert_eq!(outcome.text, "Hi! What can I help you with?");
        assert_eq!(outcome.num_turns, 1);
        assert!(outcome.total_cost_usd > 0.0);
        assert!(outcome.duration_ms >= outcome.duration_api_ms);
    }

    #[test]
    fn a_rate_limit_event_is_kept_rather_than_dropped() {
        // Neither Elixir dispatcher in this tree knows this kind exists;
        // both fall through to a catch-all. It is a real event of a real
        // run, so it is a variant here.
        let event = parsed()
            .into_iter()
            .find(|e| e.kind() == "rate_limit")
            .expect("the capture has one");
        let Event::RateLimit { raw } = event else {
            panic!("kind and variant agree");
        };
        assert!(raw.contains("five_hour"));
    }

    #[test]
    fn an_unmodelled_kind_names_itself() {
        let event = parsed()
            .into_iter()
            .find(|e| e.kind() == "unrecognized")
            .expect("the capture has one");
        let Event::Unrecognized { kind, raw } = event else {
            panic!("kind and variant agree");
        };
        assert_eq!(kind, "an_event_kind_from_the_future");
        assert!(raw.contains("synthetic"), "the line survives intact");
    }

    #[test]
    fn a_non_json_line_survives_as_evidence() {
        let event = parsed()
            .into_iter()
            .find(|e| e.kind() == "not_json")
            .expect("the capture has one");
        let Event::NotJson { line } = event else {
            panic!("kind and variant agree");
        };
        assert!(line.starts_with("not json at all"));
    }

    #[test]
    fn tool_use_and_tool_result_blocks_round_trip() {
        // Not from the capture: the smoke run had no tools enabled. The
        // shape is the one packages/mcp-ex/lib/ix_mcp/agents/cli_runner.ex
        // dispatches on.
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"secret"},
            {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},
            {"type":"tool_result","tool_use_id":"toolu_1","is_error":true},
            {"type":"a_block_kind_from_the_future"}]}}"#;
        let Event::Assistant(message) = Event::parse(line) else {
            panic!("an assistant turn");
        };
        assert_eq!(
            message.blocks,
            [
                Block::Thinking,
                Block::ToolUse {
                    name: "Bash".to_owned(),
                    id: "toolu_1".to_owned(),
                    input: r#"{"command":"ls"}"#.to_owned(),
                },
                Block::ToolResult {
                    tool_use_id: "toolu_1".to_owned(),
                    is_error: true,
                },
                Block::Other {
                    kind: "a_block_kind_from_the_future".to_owned(),
                },
            ]
        );
    }
}
