//! The restart proof, with real processes.
//!
//! `lg serve` runs as a child process. `lg chat` talks to it. The test kills
//! the daemon with SIGKILL, starts a new one on the same database, and counts
//! the offline model calls across both lives. Two more tests sign in to the
//! web pages, and stop the daemon with SIGTERM.

use std::error::Error;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
    port: u16,
}

/// A port that was free a moment ago. Tests run in parallel, so each
/// daemon needs its own port for the pages.
fn free_port() -> TestResult<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn paths() -> TestResult<Paths> {
    let dir = tempfile::tempdir()?;
    let config = dir.path().join("lg.toml");
    let port = free_port()?;
    std::fs::write(
        &config,
        format!("owner = \"tester\"\n[ui]\nbind = \"127.0.0.1:{port}\"\n"),
    )?;
    Ok(Paths {
        db: dir.path().join("lg.db"),
        socket: dir.path().join("lg.sock"),
        calls: dir.path().join("calls.log"),
        config,
        port,
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

/// One HTTP/1.1 GET to the pages, with `cookie`. Returns the head and the
/// body.
fn get(port: u16, path: &str, cookie: Option<&str>) -> TestResult<(String, String)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.into()),
        }
    };
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}Connection: close\r\n\r\n"
    )?;
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Ok((head.to_owned(), body.to_owned()))
}

/// The `name=value` of the `Set-Cookie` line of `name` in `head`.
fn cookie_of(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|line| line.strip_prefix("set-cookie: "))
        .filter_map(|value| value.split(';').next())
        .find(|pair| pair.starts_with(&format!("{name}=")))
        .map(str::to_owned)
}

#[test]
fn a_link_from_lg_ui_signs_a_browser_in() -> TestResult {
    let paths = paths()?;
    let daemon = serve(&paths)?;
    let (head, _) = get(paths.port, "/", None)?;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let output = Command::new(LG)
        .arg("--socket")
        .arg(&paths.socket)
        .arg("ui")
        .env("LG_LOG", "warn")
        .output()?;
    let printed = answer(&output)?;
    let link = printed.lines().last().unwrap_or_default();
    let base = format!("http://127.0.0.1:{}", paths.port);
    let path = link
        .strip_prefix(&base)
        .ok_or("the link names another address")?;
    let (head, _) = get(paths.port, path, None)?;
    assert!(head.starts_with("HTTP/1.1 303"), "{head}");
    let session = cookie_of(&head, "lg.sid").ok_or("no session cookie")?;
    let (head, body) = get(paths.port, "/", Some(&session))?;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        body.contains("Approvals"),
        "the home page lists the approvals"
    );
    let (again, _) = get(paths.port, path, None)?;
    assert!(again.starts_with("HTTP/1.1 403"), "a link works once");
    kill(daemon)
}

#[test]
fn sigterm_stops_the_daemon_cleanly() -> TestResult {
    let paths = paths()?;
    let mut daemon = serve(&paths)?;
    let hello = chat(&paths.socket, "m-1", "hello")?;
    assert_eq!(answer(&hello)?, "littleguy (offline): you said 'hello'");
    let status = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()?;
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    let code = loop {
        if let Some(code) = daemon.try_wait()? {
            break code;
        }
        if Instant::now() > deadline {
            daemon.kill()?;
            return Err("lg serve did not stop on SIGTERM".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(code.success(), "{code:?}");
    assert!(!paths.socket.exists(), "a clean stop removes the socket");
    // The runtime closed, so a new daemon opens the database at once.
    let next = serve(&paths)?;
    let again = chat(&paths.socket, "m-1", "hello")?;
    assert_eq!(
        answer(&again)?,
        "(already delivered) littleguy (offline): you said 'hello'"
    );
    kill(next)
}
