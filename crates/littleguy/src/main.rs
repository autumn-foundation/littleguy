//! `lg`: the littleguy command.
//!
//! - `lg serve` runs the daemon. It owns the database and the assistant.
//! - `lg chat <text>` sends one message to the daemon and prints the answer.

mod client;
mod daemon;
mod paths;
mod protocol;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use littleguy_core::{Config, OwnerName};
use littleguy_models::OfflineModel;

use crate::paths::Env;
use crate::protocol::Request;

/// An always-on personal agent on autumn-harvest.
#[derive(Debug, Parser)]
#[command(name = "lg", version, about)]
struct Cli {
    /// The daemon socket. Defaults to `<data home>/littleguy/lg.sock`.
    #[arg(long, global = true, env = "LG_SOCKET")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon.
    Serve(ServeArgs),
    /// Send one message and print the answer.
    Chat(ChatArgs),
}

/// The models that `lg serve` can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelChoice {
    /// The offline echo model. It needs no key and no network.
    Offline,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// The config file. Defaults to `<config home>/littleguy/lg.toml`.
    #[arg(long, env = "LG_CONFIG")]
    config: Option<PathBuf>,
    /// The database. Defaults to `<data home>/littleguy/lg.db`.
    #[arg(long, env = "LG_DB")]
    db: Option<PathBuf>,
    /// The model of the assistant.
    #[arg(long, value_enum, default_value = "offline")]
    model: ModelChoice,
    /// Append one line per offline model call to this file. For tests.
    #[arg(long, env = "LG_OFFLINE_CALL_LOG", hide = true)]
    offline_call_log: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ChatArgs {
    /// The message.
    text: String,
    /// The message id. A retry with the same id delivers the message once.
    #[arg(long)]
    message_id: Option<String>,
}

/// Reads the config. Without `--config` and with no default file, the owner
/// is `$USER` and every other key takes its default.
fn load_config(explicit: Option<&Path>, env: &Env) -> Result<Config> {
    if let Some(path) = explicit {
        return Ok(Config::load(path)?);
    }
    let path = env.config()?;
    if path.exists() {
        return Ok(Config::load(&path)?);
    }
    let user = std::env::var("USER").context("set USER, or write a config with `owner`")?;
    Ok(Config::for_owner(OwnerName::new(user)?))
}

async fn serve(socket: PathBuf, args: ServeArgs, env: &Env) -> Result<()> {
    let config = load_config(args.config.as_deref(), env)?;
    let db = match args.db {
        Some(db) => db,
        None => env.db()?,
    };
    let model = match args.model {
        ModelChoice::Offline => Arc::new(
            args.offline_call_log
                .map_or_else(OfflineModel::new, OfflineModel::with_call_log),
        ),
    };
    let uid = rustix::process::getuid().as_raw();
    let daemon = daemon::Daemon::open(&db, &config, model, uid)?;
    let listener = daemon::bind(&socket).await?;
    let stop = async {
        // A failed signal handler leaves only a kill to stop the daemon.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    daemon::run(daemon, listener, stop).await?;
    // The socket file goes with the daemon. A crash leaves it, and the next
    // start replaces it.
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn chat(socket: &Path, args: ChatArgs) -> Result<bool> {
    let request = Request {
        message_id: args.message_id.unwrap_or_else(client::fresh_message_id),
        text: args.text,
    };
    let response = client::send(socket, &request).await?;
    let (text, ok) = client::render(&response);
    println!("{text}");
    Ok(ok)
}

async fn run(cli: Cli) -> Result<bool> {
    let env = Env::from_process();
    let socket = match cli.socket {
        Some(socket) => socket,
        None => env.socket()?,
    };
    match cli.command {
        Command::Serve(args) => serve(socket, args, &env).await.map(|()| true),
        Command::Chat(args) => chat(&socket, args).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LG_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_config_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.toml");
        std::fs::write(&path, "owner = \"explicit\"").unwrap();
        let config = load_config(Some(&path), &Env::default()).unwrap();
        assert_eq!(config.owner.as_str(), "explicit");
    }

    #[test]
    fn the_default_config_file_is_read_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("littleguy").join("lg.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "owner = \"default\"\nwindow = 3").unwrap();
        let env = Env {
            config_home: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        let config = load_config(None, &env).unwrap();
        assert_eq!(config.owner.as_str(), "default");
        assert_eq!(config.window, 3);
    }

    #[test]
    fn a_bad_explicit_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "owner = 3").unwrap();
        assert!(load_config(Some(&path), &Env::default()).is_err());
    }

    #[test]
    fn the_cli_parses_serve_and_chat() {
        let cli = Cli::try_parse_from(["lg", "--socket", "/s", "chat", "hi", "--message-id", "m"])
            .unwrap();
        assert_eq!(cli.socket, Some(PathBuf::from("/s")));
        assert!(matches!(cli.command, Command::Chat(ChatArgs { ref text, .. }) if text == "hi"));
        let cli = Cli::try_parse_from(["lg", "serve", "--db", "/d"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Serve(ServeArgs {
                model: ModelChoice::Offline,
                ..
            })
        ));
    }
}
