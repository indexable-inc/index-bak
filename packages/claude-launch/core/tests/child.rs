//! Lifecycle against a real child process.
//!
//! The child is a shell script rather than `claude`, because the CLI needs
//! credentials no CI sandbox has. Everything under test here is this
//! crate's, not the CLI's: the spawn, the line framing, the terminal-event
//! guarantee, the strict-protocol stop, and whether dropping the stream
//! actually kills the process. A trait-shaped mock would have exercised
//! none of those.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use claude_launch_core::{Config, Error, event_stream, run};
use futures::StreamExt as _;

const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"fake-session","model":"stub","permissionMode":"plan","cwd":"/","tools":["Read"],"claude_code_version":"0.0.0"}"#;
const ASSISTANT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","num_turns":1,"duration_ms":9,"duration_api_ms":4,"total_cost_usd":0.5,"session_id":"fake-session"}"#;

/// A stand-in `claude` that runs `body` and ignores every argument.
struct Fake {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fake {
    fn new(body: &str) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("claude");
        let mut file = std::fs::File::create(&path).expect("create the stub");
        write!(file, "#!/bin/sh\n{body}\n").expect("write the stub");
        drop(file);
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        Self { _dir: dir, path }
    }

    fn config(&self) -> Config {
        Config::print("anything").bin(&self.path)
    }
}

/// Print the canned lines of a well-behaved run.
fn happy_body() -> String {
    [INIT, ASSISTANT, RESULT]
        .map(|line| format!("printf '%s\\n' '{line}'"))
        .join("\n")
}

#[tokio::test]
async fn a_run_returns_the_terminal_result() {
    let fake = Fake::new(&happy_body());
    let outcome = run(&fake.config()).await.expect("a clean run");
    assert_eq!(outcome.text, "hi");
    assert_eq!(outcome.session_id, "fake-session");
    assert!(!outcome.is_error);
}

#[tokio::test]
async fn the_last_event_is_always_exited() {
    let fake = Fake::new(&happy_body());
    let stream = event_stream(&fake.config()).expect("a spawn");
    let kinds: Vec<&'static str> = stream.map(|event| event.kind()).collect().await;
    assert_eq!(
        kinds,
        ["init", "assistant", "result", "exited"],
        "a consumer must never have to guess whether the run finished"
    );
}

#[tokio::test]
async fn an_unmodelled_event_kind_stops_a_strict_run() {
    let fake = Fake::new(&format!(
        "{}\nprintf '%s\\n' '{{\"type\":\"newly_invented\"}}'\n{}",
        [INIT].map(|l| format!("printf '%s\\n' '{l}'")).join("\n"),
        [RESULT].map(|l| format!("printf '%s\\n' '{l}'")).join("\n"),
    ));
    let error = run(&fake.config()).await.expect_err("strict by default");
    let Error::Protocol { message } = error else {
        panic!("an unmodelled kind is a protocol failure");
    };
    assert!(message.contains("newly_invented"), "{message}");
    // The message says how stale the mirror is, so the report does not
    // need someone to go and look.
    assert!(message.contains("2.1.220"), "{message}");
}

#[tokio::test]
async fn a_relaxed_run_carries_on_past_an_unmodelled_kind() {
    let fake = Fake::new(&format!(
        "{}\nprintf '%s\\n' '{{\"type\":\"newly_invented\"}}'\n{}",
        [INIT].map(|l| format!("printf '%s\\n' '{l}'")).join("\n"),
        [RESULT].map(|l| format!("printf '%s\\n' '{l}'")).join("\n"),
    ));
    let mut config = fake.config();
    config.features.strict_protocol = false;
    let outcome = run(&config).await.expect("relaxed runs finish");
    assert_eq!(outcome.text, "hi");
}

#[tokio::test]
async fn a_child_that_dies_without_a_result_reports_its_stderr() {
    let fake = Fake::new("echo 'error: unknown option --nonsense' >&2\nexit 3");
    let error = run(&fake.config()).await.expect_err("no result event");
    let Error::Exited { message } = error else {
        panic!("a child ending without a result is an exit failure");
    };
    assert!(message.contains("exit 3"), "{message}");
    assert!(message.contains("unknown option --nonsense"), "{message}");
}

#[tokio::test]
async fn a_missing_executable_is_a_spawn_failure_not_a_panic() {
    let config = Config::print("hi").bin("/definitely/not/here/claude");
    assert!(matches!(run(&config).await, Err(Error::Spawn { .. })));
}

#[tokio::test]
async fn dropping_the_stream_kills_the_child() {
    // The leak this guards against costs money: an abandoned claude keeps
    // talking to the API until it finishes on its own.
    let dir = tempfile::tempdir().expect("a temp dir");
    let pidfile = dir.path().join("pid");
    // The pid lands before the event a reader waits on, so the file is
    // there by the time the test looks.
    let fake = Fake::new(&format!(
        "echo $$ > {}\nprintf '%s\\n' '{INIT}'\nsleep 30",
        pidfile.display()
    ));
    let mut stream = Box::pin(event_stream(&fake.config()).expect("a spawn"));
    let first = stream.next().await.expect("the init event");
    assert_eq!(first.kind(), "init");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the child wrote its pid")
        .trim()
        .parse()
        .expect("a pid");
    // Without this the test passes for the wrong reason whenever the stub's
    // `sleep` is missing: the shell exits on its own and "the process is
    // gone" is true before anything is dropped.
    assert!(alive(pid), "the stub is still running before the drop");
    drop(stream);

    // The kill is asynchronous: tokio reaps on its next turn.
    for _ in 0..100 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("pid {pid} outlived the stream that spawned it");
}

/// Whether `pid` still exists.
///
/// Asked with the syscall rather than a `kill(1)` subprocess: a missing
/// binary and a dead process are the same answer from a subprocess, and one
/// of them would make this test pass for the wrong reason.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence checks without
    // delivering anything, and takes no pointer arguments.
    unsafe { libc::kill(pid, 0) == 0 }
}
