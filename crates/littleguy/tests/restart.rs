//! The restart proof, with real processes.
//!
//! `lg serve` runs as a child process. `lg chat` talks to it. The test kills
//! the daemon with SIGKILL, starts a new one on the same database, and counts
//! the offline model calls across both lives.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const LG: &str = env!("CARGO_BIN_EXE_lg");

struct Paths {
    _dir: tempfile::TempDir,
    db: PathBuf,
    socket: PathBuf,
    calls: PathBuf,
    config: PathBuf,
}

fn paths() -> TestResult<Paths> {
    let dir = tempfile::tempdir()?;
    let config = dir.path().join("lg.toml");
    std::fs::write(&config, "owner = \"tester\"\n")?;
    Ok(Paths {
        db: dir.path().join("lg.db"),
        socket: dir.path().join("lg.sock"),
        calls: dir.path().join("calls.log"),
        config,
        _dir: dir,
    })
}

fn serve_command(paths: &Paths, socket: &Path) -> Command {
    let mut command = Command::new(LG);
    command
        .arg("--socket")
        .arg(socket)
        .arg("serve")
        .arg("--config")
        .arg(&paths.config)
        .arg("--db")
        .arg(&paths.db)
        .env("LG_OFFLINE_CALL_LOG", &paths.calls)
        .env("LG_LOG", "warn");
    command
}

/// Starts `lg serve` and waits until its socket answers.
fn serve(paths: &Paths) -> TestResult<Child> {
    let child = serve_command(paths, &paths.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&paths.socket).is_err() {
        if Instant::now() > deadline {
            return Err("lg serve did not start".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(child)
}

fn chat(socket: &Path, id: &str, text: &str) -> TestResult<Output> {
    Ok(Command::new(LG)
        .arg("--socket")
        .arg(socket)
        .args(["chat", "--message-id", id, text])
        .env("LG_LOG", "warn")
        .output()?)
}

/// The trimmed stdout of a successful `lg chat`.
fn answer(output: &Output) -> TestResult<String> {
    if !output.status.success() {
        return Err(format!("lg chat failed: {output:?}").into());
    }
    Ok(String::from_utf8(output.stdout.clone())?.trim().to_owned())
}

fn calls(paths: &Paths) -> usize {
    std::fs::read_to_string(&paths.calls).map_or(0, |log| log.lines().count())
}

fn kill(mut child: Child) -> TestResult {
    // `Child::kill` sends SIGKILL: the daemon gets no chance to clean up.
    child.kill()?;
    child.wait()?;
    Ok(())
}

#[test]
fn chat_answers_and_a_killed_daemon_resumes_without_paying_twice() -> TestResult {
    let paths = paths()?;
    let first = serve(&paths)?;
    let hello = chat(&paths.socket, "m-1", "hello")?;
    assert_eq!(answer(&hello)?, "littleguy (offline): you said 'hello'");
    assert_eq!(calls(&paths), 1);
    kill(first)?;

    let second = serve(&paths)?;
    // The same id after the restart is a duplicate. Nothing runs again.
    let again = chat(&paths.socket, "m-1", "hello")?;
    assert_eq!(
        answer(&again)?,
        "(already delivered) littleguy (offline): you said 'hello'"
    );
    assert_eq!(calls(&paths), 1, "a recorded turn was paid again");

    let next = chat(&paths.socket, "m-2", "and again")?;
    assert_eq!(answer(&next)?, "littleguy (offline): you said 'and again'");
    assert_eq!(calls(&paths), 2);
    kill(second)
}

#[test]
fn chat_without_a_daemon_fails_and_names_the_socket() -> TestResult {
    let paths = paths()?;
    let output = chat(&paths.socket, "m-1", "hello")?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("lg.sock"), "{stderr}");
    Ok(())
}

#[test]
fn a_second_daemon_on_the_same_database_refuses_to_start() -> TestResult {
    let paths = paths()?;
    let first = serve(&paths)?;
    let other_socket = paths.socket.with_extension("other");
    let output = serve_command(&paths, &other_socket).output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("lock"), "{stderr}");
    kill(first)
}
