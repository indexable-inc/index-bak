//! `nix-tui` -- the terminal frontend of nix-web-monitor.
//!
//! The web monitor already owns the hard part: `nix_web_monitor_parser` turns
//! Nix's `internal-json` stream into a build model, and the ix fork's
//! `nix store builds --json` (`build-status-dir`) reports every goal on the
//! machine with a real start time and a why-chain. This binary adds no parsing
//! of its own; it renders those two, in a terminal, for people who do not want
//! to open a browser to watch a build.
//!
//!   nix-tui -- build .#ix     wrap a nix command and follow it
//!   nix-tui                   attach to whatever the daemon is already doing
//!
//! Attach mode is the mode the web UI's machine-builds panel already has and
//! the terminal did not: no wrapped command, so it shows builds started by
//! anyone, including ones that began before it did.

mod ui;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use nix_web_monitor_parser::global::GlobalBuilds;
use nix_web_monitor_parser::{BuildView, MonitorState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Everything the renderer draws. Held by the main thread only; the worker
/// threads talk to it through [`Msg`].
#[derive(Default)]
pub struct App {
    pub title: String,
    pub view: Option<BuildView>,
    pub global: Option<GlobalBuilds>,
    pub global_error: Option<String>,
    /// Derivation -> its parent in a why-chain: the in-flight dependency DAG,
    /// accumulated across polls so an edge survives the goal that revealed it.
    pub dag_parent: BTreeMap<String, String>,
    pub dag_roots: BTreeMap<String, ()>,
    pub finished: Option<String>,
}

enum Msg {
    /// A parsed stderr line from the wrapped nix command, already folded into
    /// the state machine on the reader thread.
    View(Box<BuildView>),
    Global(Result<GlobalBuilds, String>),
    ChildExit(String),
    Key(KeyCode),
}

struct Args {
    snapshot: Option<Duration>,
    poll: Duration,
    nix_args: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        snapshot: None,
        poll: Duration::from_millis(800),
        nix_args: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                a.nix_args = it.collect();
                break;
            }
            "--snapshot" => {
                a.snapshot = Some(Duration::from_secs_f64(
                    it.next().context("--snapshot needs seconds")?.parse()?,
                ));
            }
            "--poll" => {
                a.poll =
                    Duration::from_secs_f64(it.next().context("--poll needs seconds")?.parse()?);
            }
            "-h" | "--help" => {
                println!(
                    "nix-tui [--poll SECS] [--snapshot SECS] [-- <nix args>...]\n\n\
                     With nix args, runs `nix <args> --log-format internal-json` and follows it.\n\
                     With none, attaches to the builds already in progress on this machine.\n\
                     --snapshot renders offscreen for N seconds and prints the final frame as text."
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}; pass nix arguments after --"),
        }
    }
    Ok(a)
}

/// Spawn `nix <args> --log-format internal-json`, feed every stderr line to a
/// `MonitorState` on the reader thread, and ship the projected `BuildView`.
/// Projecting on the reader thread rather than sending raw lines keeps the
/// parser's state single-owner, exactly as the server and the ndjson emitter do.
fn spawn_nix(nix_args: &[String], tx: Sender<Msg>) -> Result<()> {
    let label = format!("nix {}", nix_args.join(" "));
    let mut command = Command::new("nix");
    command
        .arg("--log-format")
        .arg("internal-json")
        .args(nix_args)
        // Nix's own stdout is the command's real output; it must not land on
        // the alternate screen mid-frame.
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("spawning nix")?;
    let stderr = child.stderr.take().context("nix stderr was not captured")?;

    thread::spawn(move || {
        let mut state = MonitorState::new(label);
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut reader = BufReader::new(stderr);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            // `read_until`, not `lines()`: it yields a final unterminated line
            // at EOF, and it does not abort the stream on a builder's non-UTF-8
            // byte (which would then block the child on a full stderr pipe).
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            state.apply_line(&String::from_utf8_lossy(&buf));
            // Throttle: a chatty build emits thousands of lines a second and
            // the screen redraws at most ~10 times a second anyway.
            if last.elapsed() >= Duration::from_millis(120) {
                if tx.send(Msg::View(Box::new(state.build_view()))).is_err() {
                    return;
                }
                last = Instant::now();
            }
        }
        let code = child.wait().ok().and_then(|s| s.code());
        state.finish(code);
        let _ = tx.send(Msg::View(Box::new(state.build_view())));
        let _ = tx.send(Msg::ChildExit(match code {
            Some(0) => "nix finished successfully".to_owned(),
            Some(c) => format!("nix exited {c}"),
            None => "nix was killed by a signal".to_owned(),
        }));
    });
    Ok(())
}

/// Poll `nix store builds --json`, the machine-wide goal list, and decode it
/// with the parser crate's tolerant wire types.
fn spawn_global(poll: Duration, tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let result = Command::new("nix")
                .args([
                    "store",
                    "builds",
                    "--json",
                    "--extra-experimental-features",
                    "build-status-dir",
                ])
                .output()
                .map_err(|e| e.to_string())
                .and_then(|out| {
                    if out.status.success() {
                        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
                    } else {
                        // Stock nix prints "unknown command"; say so rather
                        // than render an empty machine as an idle one.
                        Err(String::from_utf8_lossy(&out.stderr)
                            .lines()
                            .next()
                            .unwrap_or("nix store builds failed")
                            .to_owned())
                    }
                })
                .map(
                    |builds: Vec<nix_web_monitor_parser::global::GlobalBuild>| GlobalBuilds {
                        detected: true,
                        builds,
                        status: String::new(),
                    },
                );
            if tx.send(Msg::Global(result)).is_err() {
                return;
            }
            thread::sleep(poll);
        }
    });
}

fn spawn_keys(tx: Sender<Msg>) {
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if let Event::Key(k) = ev
                && k.kind == KeyEventKind::Press
                && tx.send(Msg::Key(k.code)).is_err()
            {
                return;
            }
        }
    });
}

/// Drain queued messages for up to `budget`, returning whether to quit.
fn pump(app: &mut App, rx: &Receiver<Msg>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let mut quit = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(msg) = (if left.is_zero() {
            rx.try_recv().map_err(|_| ())
        } else {
            rx.recv_timeout(left).map_err(|_| ())
        }) else {
            break;
        };
        match msg {
            Msg::View(v) => app.view = Some(*v),
            Msg::Global(Ok(g)) => {
                for build in &g.builds {
                    for pair in build.why.chain.windows(2) {
                        app.dag_parent.insert(pair[1].clone(), pair[0].clone());
                    }
                    if let Some(root) = build.why.chain.first() {
                        app.dag_roots.insert(root.clone(), ());
                    }
                }
                app.global_error = None;
                app.global = Some(g);
            }
            Msg::Global(Err(e)) => app.global_error = Some(e),
            Msg::ChildExit(s) => {
                app.finished = Some(s);
                quit = true;
            }
            Msg::Key(KeyCode::Char('q') | KeyCode::Esc) => quit = true,
            Msg::Key(_) => {}
        }
    }
    quit
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let (tx, rx) = channel();

    let mut app = App {
        title: if args.nix_args.is_empty() {
            "attached to the local daemon".to_owned()
        } else {
            format!("nix {}", args.nix_args.join(" "))
        },
        ..App::default()
    };

    spawn_global(args.poll, tx.clone());
    if !args.nix_args.is_empty() {
        spawn_nix(&args.nix_args, tx.clone())?;
    }

    if let Some(dur) = args.snapshot {
        // Offscreen: no raw mode and no alternate screen, so there is nothing
        // to restore and the frame can be captured into a file or a report.
        let mut term = Terminal::new(TestBackend::new(150, 34))?;
        let end = Instant::now() + dur;
        while Instant::now() < end {
            pump(&mut app, &rx, Duration::from_millis(200));
            term.draw(|f| ui::draw(f, &app, now_ms()))?;
        }
        term.draw(|f| ui::draw(f, &app, now_ms()))?;
        print!("{}", frame_text(term.backend()));
        return Ok(());
    }

    spawn_keys(tx);

    // `ratatui::init` installs a panic hook that leaves raw mode and the
    // alternate screen; the explicit restore covers the ordinary exit. Both are
    // needed -- a panic inside draw() with raw mode still set leaves a shell
    // that echoes nothing.
    let mut term = ratatui::init();
    let outcome = run(&mut term, &mut app, &rx);
    ratatui::restore();
    if let Some(msg) = &app.finished {
        println!("{msg}");
    }
    outcome
}

fn run<B>(term: &mut Terminal<B>, app: &mut App, rx: &Receiver<Msg>) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: Send + Sync + 'static,
{
    loop {
        let quit = pump(app, rx, Duration::from_millis(100));
        term.draw(|f| ui::draw(f, app, now_ms()))?;
        if quit {
            // Leave the final frame on screen long enough to read the outcome.
            thread::sleep(Duration::from_millis(500));
            return Ok(());
        }
    }
}

/// Flatten a `TestBackend` buffer to plain text with trailing blanks trimmed.
fn frame_text(backend: &TestBackend) -> String {
    let buffer = backend.buffer();
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
