//! Spawn a child and turn its stdout into [`Event`]s.

use std::process::Stdio;

use futures::Stream;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::config::{Config, Error, InputFormat, OutputFormat};
use crate::event::{Event, Outcome, SCHEMA_CHECKED};

/// How much of the child's stderr to keep for the failure report. Enough for
/// a usage message and a stack trace, small enough that a child looping on
/// an error cannot grow the BEAM's heap.
const STDERR_CAP: u64 = 64 * 1024;

/// Spawn `claude` under `config` and stream its events.
///
/// The stream is lazy in the sense that matters on the BEAM: it produces one
/// event per granted credit, so a consumer that stops reading stops the
/// child. Dropping the stream kills the child (`kill_on_drop`), which is
/// what makes a caller exiting mid-run a real cancellation rather than an
/// orphaned process.
///
/// The last event is always [`Event::Exited`], on every path.
///
/// Must be called from inside a tokio runtime context: spawning a
/// `tokio::process::Child` registers it with the reactor. The Elixir binding
/// enters unibind's shared runtime first.
///
/// # Errors
///
/// - [`Error::Config`] when [`Config::validate`] refuses, when the output
///   format is not stream-json (there would be nothing to parse), or when
///   the input format is stream-json (this surface owns stdin and has
///   nowhere to accept turns; use [`Config::argv`] and drive the child
///   yourself).
/// - [`Error::Spawn`] when the child could not be started.
pub fn event_stream(config: &Config) -> Result<impl Stream<Item = Event> + Send + 'static, Error> {
    if !matches!(config.output_format, OutputFormat::StreamJson) {
        return Err(Error::Config {
            message: format!(
                "event_stream parses stream-json; output_format is {}",
                config.output_format.as_str()
            ),
        });
    }
    if matches!(config.input_format, InputFormat::StreamJson) {
        return Err(Error::Config {
            message: "event_stream closes the child's stdin, so it cannot carry \
                      stream-json input turns; render the argv with Config::argv and own \
                      the process, the way a caller that needs the injection channel does"
                .to_owned(),
        });
    }
    let producer = Producer::spawn(config)?;
    Ok(futures::stream::unfold(producer, |producer| async move {
        // `unfold` insists on a bare tuple; `Step` names the halves
        // everywhere else.
        producer
            .step()
            .await
            .map(|step| (step.event, step.producer))
    }))
}

/// Run to completion and return the terminal result.
///
/// # Errors
///
/// Everything [`event_stream`] can raise, plus [`Error::Protocol`] when the
/// child emitted an event kind this crate does not model and
/// `Features::strict_protocol` is on, and [`Error::Exited`] when the child
/// ended without a terminal `result` event.
pub async fn run(config: &Config) -> Result<Outcome, Error> {
    let stream = event_stream(config)?;
    futures::pin_mut!(stream);
    let mut outcome: Option<Outcome> = None;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        match event {
            Event::Result(result) => outcome = Some(result),
            Event::Unrecognized { kind, raw } if config.features.strict_protocol => {
                return Err(Error::Protocol {
                    message: format!(
                        "unmodelled stream-json event `{kind}`; this crate's mirror was last \
                         checked against {SCHEMA_CHECKED}, so it is probably out of date. \
                         The line was: {raw}"
                    ),
                });
            }
            Event::Exited { code, stderr } => {
                return outcome.ok_or_else(|| Error::Exited {
                    message: exit_message(code, &stderr),
                });
            }
            _ => {}
        }
    }
    // Unreachable while `Producer::step` keeps its promise that the last
    // event is always `Exited`; reported rather than asserted because a
    // panic inside a NIF takes the scheduler thread with it.
    outcome.ok_or_else(|| Error::Exited {
        message: "the event stream ended without an Exited event".to_owned(),
    })
}

/// Describe a child that ended without saying anything useful.
fn exit_message(code: Option<i32>, stderr: &str) -> String {
    let status = code.map_or_else(
        || "killed by a signal".to_owned(),
        |code| format!("exit {code}"),
    );
    if stderr.is_empty() {
        format!("claude ended without a result event ({status}) and wrote nothing to stderr")
    } else {
        format!("claude ended without a result event ({status}): {stderr}")
    }
}

/// One step of the producer: the event to hand out and the producer to
/// continue from.
///
/// A named pair rather than a tuple, because `unfold`'s `(item, state)`
/// says nothing about which half is which at the two places that build one.
struct Step {
    event: Event,
    producer: Producer,
}

/// The running child and the position in its output.
struct Producer {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    /// The task draining the child's stderr, joined at the end of the run.
    /// Joined rather than sampled: a detached task that writes into a shared
    /// buffer races the exit, and the race is one-sided -- the report comes
    /// out empty exactly when there was something to say.
    stderr: Option<JoinHandle<String>>,
    /// Trouble on this side of the pipe, appended to the child's own stderr
    /// in the terminal event.
    note: String,
    /// Set once the run is over; the next step yields nothing.
    done: bool,
    /// Set when an unrecognized event was just emitted under
    /// `strict_protocol`: the next step ends the run rather than reading on.
    stopping: bool,
    strict: bool,
}

impl Drop for Producer {
    fn drop(&mut self) {
        // The child dies with the `Child` (`kill_on_drop`); the reader has
        // to be told, or it sits on a pipe a grandchild still holds open
        // long after the run everybody thinks is cancelled.
        if let Some(handle) = self.stderr.take() {
            handle.abort();
        }
    }
}

impl Producer {
    /// Start the child and take its pipes.
    fn spawn(config: &Config) -> Result<Self, Error> {
        let rendered = config.argv()?;
        let (program, args) = rendered.split_first().ok_or_else(|| Error::Config {
            message: "empty argv".to_owned(),
        })?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(config.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child outlives this call only as long as the stream does.
            // Without this a cancelled consumer leaves a claude session
            // running against the API, billing, until it finishes alone.
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|error| Error::Spawn {
            message: format!("could not start `{program}`: {error}"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Spawn {
            message: "the child was spawned without a stdout pipe".to_owned(),
        })?;
        let stderr = child.stderr.take().map(|pipe| {
            tokio::spawn(async move {
                let mut text = String::new();
                // Drained as it arrives rather than after the exit: a chatty
                // child otherwise blocks on a full pipe, deadlocking against
                // a consumer waiting on stdout.
                let _ = pipe.take(STDERR_CAP).read_to_string(&mut text).await;
                text
            })
        });
        Ok(Self {
            child,
            lines: BufReader::new(stdout).lines(),
            stderr,
            note: String::new(),
            done: false,
            stopping: false,
            strict: config.features.strict_protocol,
        })
    }

    /// The next event, or `None` once the stream is over.
    async fn step(mut self) -> Option<Step> {
        if self.done {
            return None;
        }
        if self.stopping {
            return Some(self.finish().await);
        }
        loop {
            return match self.lines.next_line().await {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => {
                    let event = Event::parse(&line);
                    self.stopping = self.strict && matches!(event, Event::Unrecognized { .. });
                    Some(Step {
                        event,
                        producer: self,
                    })
                }
                Ok(None) => Some(self.finish().await),
                Err(error) => {
                    // A read failure on the child's stdout is the end of the
                    // run whatever the child does next; report it where the
                    // exit status would have gone rather than dropping it.
                    self.note = format!("\nreading stdout failed: {error}");
                    Some(self.finish().await)
                }
            };
        }
    }

    /// Reap the child and emit the terminal event.
    async fn finish(mut self) -> Step {
        // A failed `wait` and a signal death both leave no exit code, and
        // reporting them identically is how an ECHILD from an auto-reaped
        // child reads as "killed by a signal" -- which is exactly what it
        // did on Linux before `ensure_sigchld_default` went back into the
        // binding. Keep the failure's text so the two stay distinguishable.
        let code = match self.child.wait().await {
            Ok(status) => status.code(),
            Err(error) => {
                self.note
                    .push_str(&format!("\nwaiting for the child failed: {error}"));
                None
            }
        };
        // The child has exited, so every writer on the stderr pipe is gone
        // and the reader is at EOF; joining it here is a formality that
        // cannot hang for longer than the stdout read already did.
        let mut stderr = match self.stderr.take() {
            Some(handle) => handle.await.unwrap_or_default(),
            None => String::new(),
        };
        stderr.push_str(&self.note);
        self.done = true;
        Step {
            event: Event::Exited { code, stderr },
            producer: self,
        }
    }
}
