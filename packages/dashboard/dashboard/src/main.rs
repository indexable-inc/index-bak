//! Standalone aggregator: one web canvas for every resource producer on the
//! machine.
//!
//! Each producing process exposes its panes over a unix socket in the discovery
//! directory ([`discovery_dir`](dashboard_core::discovery_dir)); see the producer
//! side in [`dashboard_core::Publisher`] (the `tui` crate adapts its PTY manager
//! into terminal panes, a VM controller publishes an HTML or data pane, and so
//! on). This binary scans that directory, connects to every socket, folds each
//! producer's stream into one Loro document under its own scope, and serves the
//! shared board over HTTP + SSE. No producer owns the server and exactly one
//! process binds a TCP port, so any number of producers can come and go behind
//! one stable URL.
//!
//! `dashboard demo` runs a self-contained producer that publishes one pane of
//! each kind, including the writable `input` pane, so the canvas can be
//! exercised with no other process running.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dashboard_core::{
    Actor, ExecTraceLine, ExecView, Hub, InputEntry, InputLine, InputRouter, InputWatcher, Pane,
    ProducerEvent, Publisher, RecordingStore, TerminalView, discovery_dir, serve_hub, socket_path,
    subscribe_bidi,
};
use tokio::sync::mpsc;

/// Aggregate every ix resource producer socket into one live web canvas.
#[derive(Parser)]
#[command(name = "dashboard", version, about)]
struct Cli {
    /// Address to bind the dashboard on. `0.0.0.0` exposes it on the network.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port to bind. `0` picks an ephemeral port, printed on startup.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Directory of producer sockets to watch (serve) or publish into (demo).
    /// Defaults to the ix discovery directory (`$IX_DASH_DIR`,
    /// `$XDG_RUNTIME_DIR/ix-dash`, or `/tmp/ix-dash-*`). Global so it works
    /// before or after the subcommand.
    #[arg(long, global = true)]
    dir: Option<PathBuf>,

    /// How often to rescan the directory for new or removed sockets, in
    /// milliseconds.
    #[arg(long, default_value_t = 500)]
    rescan_ms: u64,

    /// How often to persist the live board as a replayable recording, in
    /// milliseconds. `0` disables on-disk recording (replay still works for the
    /// current browser session from the live stream).
    #[arg(long, default_value_t = 5000)]
    record_ms: u64,

    /// Directory recordings are written to. Defaults to the ix recordings
    /// directory (`$IX_DASH_RECORDINGS`, `$XDG_STATE_HOME/ix-dash/recordings`,
    /// or `~/.local/state/ix-dash/recordings`).
    #[arg(long)]
    record_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Publish one pane of every kind (terminal, html, exec, data, and a
    /// writable input) to the discovery directory until interrupted, for
    /// exercising the canvas standalone.
    Demo,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Demo) => run_demo(cli.dir).await,
        None => run_server(&cli).await,
    }
}

/// Serve the aggregated canvas and watch the discovery directory.
async fn run_server(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dir = cli.dir.clone().unwrap_or_else(discovery_dir);
    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;

    let hub = Hub::new();
    // Say who this writer is up front, so the aggregator's own commits (pane
    // folds, auto-declared compose notes) are attributed in the history UI
    // instead of arriving as an anonymous peer id.
    hub.declare_identity(&Actor::agent("dashboard-aggregator"));
    let handle = tokio::runtime::Handle::current();

    // Persist the live board so a session survives a restart and can be shared.
    // A store failure (e.g. an unwritable directory) is not fatal: the dashboard
    // still serves live, and replay works from the browser's own history.
    let recordings = match recording_store(cli) {
        Ok(store) => Some(Arc::new(store)),
        Err(error) => {
            eprintln!("dashboard: recordings disabled ({error})");
            None
        }
    };

    // The process runtime outlives the dashboard, so the server and discovery
    // loop spawned on it run for the lifetime of the binary.
    let served = serve_hub(hub.clone(), addr, recordings.clone(), &handle).await?;
    let mut dashboard = served.dashboard;
    // Held until shutdown so the server keeps running for the binary's lifetime.
    let _stop_rx = served.shutdown;
    println!(
        "dashboard: serving {}  (watching {})",
        dashboard.url(),
        dir.display()
    );

    // Held until shutdown so a final snapshot captures the last interval of
    // changes, which the periodic recorder task would otherwise lose when it is
    // aborted on stop.
    let recording_session = recordings
        .as_ref()
        .filter(|_| cli.record_ms > 0)
        .map(|store| {
            let recorder =
                store.spawn_recorder(hub.clone(), Duration::from_millis(cli.record_ms), &handle);
            println!(
                "dashboard: recording to {} ({})",
                store.dir().display(),
                recorder.id
            );
            dashboard.push_task(recorder.task);
            (store.clone(), recorder.id)
        });

    // Discover producers, fold each event into the hub, and route viewer
    // inputs back the other way. The transport (directory scan, per-socket
    // read, stale-socket reaping, the return channel) lives in
    // `dashboard_core::subscribe`, shared with the other consumer
    // (`ix-windows`, which uses the event half alone).
    let feed = subscribe_bidi(dir, Duration::from_millis(cli.rescan_ms), &handle);
    let discovery = tokio::spawn(fold_events(hub.clone(), feed.events, feed.inputs.clone()));
    dashboard.push_task(discovery);
    let routing = tokio::spawn(route_inputs(hub.watch_inputs(), feed.inputs));
    dashboard.push_task(routing);

    tokio::signal::ctrl_c().await?;
    println!("\ndashboard: shutting down");
    dashboard.stop().await;
    // The periodic recorder task was aborted by `stop`; write one last snapshot
    // now that the document is final, so the recording does not lose the last
    // interval of changes before exit.
    if let Some((store, id)) = recording_session {
        let _ = store.save(&id, &hub.export_snapshot());
    }
    Ok(())
}

/// Fold producer events into the hub, replaying a producer's scoped inputs
/// back to it whenever it (re)appears.
///
/// Replay is what closes the offline gap: an answer written while a producer
/// was down (or before this aggregator found it) sits in the document, and
/// the producer receives it with its first snapshot. A value can therefore
/// arrive twice -- once live through `route_inputs`, once replayed -- which
/// is why an input value that triggers an action carries its own id for the
/// producer to dedup on (the `send` convention, `{id, text}`).
async fn fold_events(hub: Arc<Hub>, mut events: mpsc::Receiver<ProducerEvent>, router: InputRouter) {
    let mut live: HashSet<String> = HashSet::new();
    while let Some(event) = events.recv().await {
        match event {
            ProducerEvent::Snapshot(snapshot) => {
                if live.insert(snapshot.producer.clone()) {
                    replay_inputs(&hub, &router, &snapshot.producer);
                }
                hub.apply_scope(&snapshot.producer, &snapshot.panes);
            }
            ProducerEvent::Gone { producer } => {
                live.remove(&producer);
                hub.remove_scope(&producer);
            }
        }
    }
}

/// Send every input under `producer`'s scope back to it.
fn replay_inputs(hub: &Hub, router: &InputRouter, producer: &str) {
    for entry in hub.inputs() {
        if entry.scope == producer {
            route_entry(router, entry);
        }
    }
}

/// Route each changed input to the producer whose scope owns it.
async fn route_inputs(mut watcher: InputWatcher, router: InputRouter) {
    while let Some(batch) = watcher.changed().await {
        for entry in batch {
            route_entry(&router, entry);
        }
    }
}

/// One entry to its producer: the scope half of the input key names the
/// producer, and the rest is the wire line. A refused route is the
/// disconnected-producer case `replay_inputs` exists for, not an error to
/// surface per entry.
fn route_entry(router: &InputRouter, entry: InputEntry) {
    let InputEntry {
        scope,
        pane,
        field,
        value,
    } = entry;
    let _ = router.route(&scope, InputLine { pane, field, value });
}

/// Open the recordings store at the configured directory, or the default one.
fn recording_store(cli: &Cli) -> Result<RecordingStore, Box<dyn std::error::Error>> {
    let store = match cli.record_dir.clone() {
        Some(dir) => RecordingStore::new(dir)?,
        None => RecordingStore::open_default()?,
    };
    Ok(store)
}

/// Run a demo producer: publish one pane of every kind, each ticking once a
/// second, until interrupted. Exercises the whole pipeline (publisher socket,
/// aggregator fold, every renderer) with no other process.
async fn run_demo(dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.map_or_else(socket_path, |d| {
        d.join(format!("{}-demo.sock", std::process::id()))
    });
    let mut publisher = Publisher::bind(path.clone(), &tokio::runtime::Handle::current())?;
    println!(
        "dashboard demo: publishing {} panes on {} (run `dashboard` in another shell)",
        demo_panes(0).len(),
        path.display()
    );

    let mut tick: u64 = 0;
    loop {
        publisher.publish(&demo_panes(tick));
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(1)) => tick += 1,
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
        }
    }
    println!("\ndashboard demo: shutting down");
    publisher.stop().await;
    Ok(())
}

/// The demo's panes at a given tick: one of every kind, including the writable
/// input pane.
fn demo_panes(tick: u64) -> Vec<Pane> {
    // `(tick % 20) + 1` is in `1..=20`, so it always fits in `usize`.
    let bar = "#".repeat((tick % 20) as usize + 1);
    let terminal = Pane::terminal(
        "demo-term",
        TerminalView {
            command: "demo".to_owned(),
            args: "--tick".to_owned(),
            rows: 3,
            cols: 40,
            alive: true,
            // A green "tick" line, exercising the SGR renderer.
            screen: format!("\x1b[32mtick {tick}\x1b[0m\n{bar}\nany resource is a pane"),
            scrollback: format!("previous tick {}", tick.saturating_sub(1)),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            cursor_shape: "block".to_owned(),
            exit_code: None,
            status: None,
            agent: None,
        },
    );
    let html = Pane::html(
        "demo-html",
        "html pane",
        format!(
            "<div style=\"font:14px ui-monospace,monospace;padding:14px;color:#89b4fa\">\
             <div style=\"font-size:28px\">{tick}</div>\
             <div style=\"opacity:.6\">a producer-rendered HTML view</div></div>"
        ),
    );
    let data = Pane::data(
        "demo-data",
        "data pane",
        "kv",
        serde_json::json!({
            "tick": tick,
            "status": if tick.is_multiple_of(2) { "even" } else { "odd" },
            // `tick % 100` is in `0..=99`, so the `as u32` is lossless.
            "load": (f64::from((tick % 100) as u32) / 100.0),
            "nested": {"a": 1, "b": [1, 2, 3]},
        }),
    );
    // An exec pane: alternate running and finished so the demo shows both states
    // (the running spinner and a finished run). The finished run carries an
    // inline-trace mapping — output paired with the line that printed it — so the
    // demo also exercises the inline-trace view (see `ExecView::trace`).
    let running = tick.is_multiple_of(2);
    let body = format!("{tick}.0\n{tick}.1\n{tick}.2\n");
    let exec = Pane::exec(
        "demo-exec",
        ExecView {
            source: format!("for i in range(3):\n    print(f\"{tick}.{{i}}\")"),
            lang: "python".to_owned(),
            stdout: if running { String::new() } else { body.clone() },
            stderr: String::new(),
            result: String::new(),
            running,
            ok: if running { None } else { Some(true) },
            duration_ms: if running { None } else { Some(420) },
            topic: Some("demo".to_owned()),
            line: if running { Some(2) } else { None },
            error_line: None,
            // The loop's prints all come from the second source line.
            trace: if running {
                Vec::new()
            } else {
                vec![ExecTraceLine {
                    line: 2,
                    text: body,
                }]
            },
        },
    );
    // An input pane: a `data` pane with the `input` renderer, the only kind a
    // viewer can WRITE (see `InputChoice.svelte` for the body shape). Clicking an
    // option writes the answer into the document's `inputs` map, so the demo
    // gives the write path added in #4192 a surface a human can click with no
    // other process running. The body does not tick: the answer is stored under
    // the pane's key outside the pane, so a changing body would only churn the
    // text diff without changing what is being asked. Nothing here reads the
    // answer back -- a `Publisher` only pushes -- so what the demo shows is the
    // click landing in the document, not a producer reacting to it.
    let input = Pane::data(
        "demo-input",
        "input pane",
        "input",
        serde_json::json!({
            "prompt": "Splice the branch?",
            "options": [
                {"value": "splice", "label": "Splice"},
                {"value": "hold", "label": "Hold", "tone": "danger"},
            ],
        }),
    );
    vec![terminal, html, exec, data, input]
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use dashboard_core::{Hub, Input, Pane, Publisher, subscribe_bidi};

    use super::{fold_events, route_inputs};

    /// The whole return path this binary wires: a viewer input lands in the
    /// hub, `route_inputs` delivers it to the producer whose scope owns it,
    /// and a producer found *after* the input exists gets it replayed on its
    /// first snapshot.
    #[tokio::test(flavor = "multi_thread")]
    async fn inputs_route_live_and_replay_on_connect() {
        let dir = std::env::temp_dir().join(format!("ix-dash-agg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("p.sock");
        let handle = tokio::runtime::Handle::current();

        let mut publisher = Publisher::bind(path.clone(), &handle).expect("bind");
        let mut inputs = publisher.inputs().expect("inputs taken once");
        publisher.publish(&[Pane::html("agent-1", "t", "<b>hi</b>")]);
        let producer = publisher.producer_id().to_owned();

        let hub = Hub::new();
        let feed = subscribe_bidi(dir.clone(), Duration::from_millis(20), &handle);
        let fold = tokio::spawn(fold_events(hub.clone(), feed.events, feed.inputs.clone()));
        let route = tokio::spawn(route_inputs(hub.watch_inputs(), feed.inputs.clone()));

        // Wait until discovery folded the producer's snapshot into the hub;
        // its write half was registered before that event was sent, so the
        // input below cannot race the registration.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let seen = hub.history().iter().any(|change| {
                change
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains(&producer))
            });
            if seen {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the producer never reached the hub"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // A viewer input under the producer's scope. `declare_note` is the
        // one hub-side write that creates an input without standing up a
        // browser document; the value fidelity of a browser write is covered
        // by dashboard-core's own tests.
        hub.declare_note(&producer, "agent-1", "compose")
            .expect("declare");

        let line = tokio::time::timeout(Duration::from_secs(5), inputs.recv())
            .await
            .expect("the routed input must arrive")
            .expect("publisher alive");
        assert_eq!(line.pane, "agent-1");
        assert_eq!(line.field, "compose");
        assert_eq!(
            line.value,
            Input::Note {
                text: String::new()
            }
        );

        // An aggregator restart: a fresh feed reconnects to the same living
        // producer and must replay the scoped input it has never routed.
        fold.abort();
        route.abort();
        let feed = subscribe_bidi(dir.clone(), Duration::from_millis(20), &handle);
        let refold = tokio::spawn(fold_events(hub.clone(), feed.events, feed.inputs.clone()));

        let replayed = tokio::time::timeout(Duration::from_secs(5), inputs.recv())
            .await
            .expect("the replayed input must arrive")
            .expect("publisher alive");
        assert_eq!(replayed.pane, "agent-1");
        assert_eq!(replayed.field, "compose");

        refold.abort();
        publisher.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
