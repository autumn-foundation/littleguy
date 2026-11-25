//! `lg`: the littleguy command.
//!
//! - `lg serve` runs the daemon. It owns the database and the assistant.
//! - `lg chat <text>` sends one message to the daemon and prints the answer.
//! - `lg pair <code>`, `lg pairs` and `lg unpair <sender>` manage who may
//!   talk to the assistant on Discord and Slack.
//! - `lg connect <integration>`, `lg connections` and `lg disconnect` manage
//!   the OAuth accounts that San Mateo keeps tokens for.
//!
//! With `DISCORD_BOT_TOKEN` set, `lg serve` also answers Discord direct
//! messages. With `SLACK_BOT_TOKEN` and `SLACK_APP_TOKEN` set, it answers
//! Slack direct messages over Socket Mode. With `SLACK_BOT_TOKEN`,
//! `SLACK_SIGNING_SECRET` and `[hooks]`, Slack calls the hooks instead.
//! Each token comes from the environment only. Each `[[camel.route]]` of
//! the config feeds the assistant too.
//!
//! `lg serve --model claude`, `openai` or `openrouter` talks to a hosted
//! model. Each reads its key as a house key: sealed with `lg keys`, or from
//! the environment (ADR 0040). The key never shows in a process list.
//!
//! - `lg keys set`, `import`, `list`, `remove` and `reseal` keep the house
//!   keys sealed with the ring of `LG_SAN_MATEO_KEY`, so the environment
//!   holds one secret.
//!
//! Each integration is a cargo feature ([`features`]). A config that needs
//! a feature that this build leaves out fails at start.

mod approvals;
mod brief;
#[cfg(feature = "camel")]
mod camel;
#[cfg(not(feature = "camel"))]
#[path = "camel_off.rs"]
mod camel;
mod chats;
mod client;
#[cfg(feature = "self-debug")]
mod config_view;
mod daemon;
mod features;
#[cfg(feature = "fleet")]
mod fleet;
mod fnv;
mod guard;
mod house_keys;
mod memory;
#[cfg(feature = "notes")]
mod notes;
#[cfg(feature = "oauth")]
mod oauth;
mod pairs;
#[cfg(feature = "patches")]
mod patches;
mod paths;
mod plans;
mod protocol;
#[cfg(feature = "refine")]
mod refine;
#[cfg(feature = "reflection")]
mod reflection;
#[cfg(feature = "research")]
mod research;
#[cfg(feature = "self-debug")]
mod runlog;
mod sandbox;
#[cfg(feature = "scripts")]
mod scripts;
#[cfg(feature = "skills")]
mod skills;
mod stamp;
mod tools;
#[cfg(feature = "self-debug")]
mod trace;
mod wake;
mod web;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use autumn_harvest_agent::AgentModel;
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(feature = "discord")]
use littleguy_channels::{DiscordRest, discord};
#[cfg(feature = "slack")]
use littleguy_channels::{SlackApi, slack};
use littleguy_core::{Config, OwnerName};
#[cfg(feature = "claude")]
use littleguy_models::ClaudeModel;
use littleguy_models::OfflineModel;
#[cfg(feature = "openai")]
use littleguy_models::{OpenAiModel, Provider};

use crate::paths::Env;
use crate::protocol::{ExpectedActive, Request, Response};

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
    /// Let the stranger who got `code` talk to the assistant.
    Pair {
        /// The code from the pairing message, such as `ABCD-1234`.
        code: String,
    },
    /// List the senders that may talk to the assistant.
    Pairs,
    /// Take a paired sender off its channel.
    Unpair {
        /// The sender id, such as `discord:123456789012345678`.
        sender: String,
    },
    /// Print a link that connects an account of a San Mateo integration:
    /// an OAuth sign-in, or a page that takes an API key.
    Connect {
        /// The integration key from the config, such as `google`.
        integration: String,
    },
    /// List the OAuth connections.
    Connections,
    /// Delete one OAuth connection.
    Disconnect {
        /// The integration key.
        integration: String,
        /// The connection id, from `lg connections`.
        connection: String,
    },
    /// List the calls that wait for an approval.
    Approvals,
    /// List the past conversations of the web and the CLI, or show one by
    /// its id. It reads only: it cannot continue an old one.
    History {
        /// The id of the conversation to show.
        id: Option<i64>,
    },
    /// Run the call that waits as request `id`.
    Approve {
        /// The request number, from `lg approvals`.
        id: i64,
    },
    /// Refuse the call that waits as request `id`.
    Deny {
        /// The request number, from `lg approvals`.
        id: i64,
        /// Why, for the assistant.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Print a one-time link that signs a browser in to the web pages.
    Ui,
    /// Read the config file again. Most sections apply at once.
    Reload,
    /// Start a new conversation. Memory, notes and the workspace stay.
    New,
    /// List the patches that the assistant proposed, and their PRs.
    Patches,
    /// Show, open or decline one patch of the assistant.
    #[command(subcommand)]
    Patch(PatchCommand),
    /// List the saved scripts, the drafts that wait and the recent runs.
    Scripts,
    /// Show, accept, decline or retire one saved script.
    #[command(subcommand)]
    Script(ScriptCommand),
    /// Propose, list and activate profile overlays of the assistant.
    #[command(subcommand)]
    Refine(RefineCommand),
    /// Seal the house keys, such as the model keys, with the key ring of
    /// `LG_SAN_MATEO_KEY`. A change applies at the next start.
    #[cfg(feature = "sealed-keys")]
    Keys(KeysArgs),
}

/// The arguments of `lg keys`.
#[cfg(feature = "sealed-keys")]
#[derive(Debug, clap::Args)]
struct KeysArgs {
    /// The database. The sealed file lives beside it.
    #[arg(long, env = "LG_DB")]
    db: Option<PathBuf>,
    /// The config file, which names more variables for `import`.
    #[arg(long, env = "LG_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: house_keys::cli::KeysCommand,
}

/// Runs one `lg keys` command on standard input, and prints its text.
#[cfg(feature = "sealed-keys")]
fn keys(args: KeysArgs, env: &Env) -> Result<bool> {
    let db = match args.db {
        Some(db) => db,
        None => env.db()?,
    };
    let config = match &args.command {
        house_keys::cli::KeysCommand::Import { names } if names.is_empty() => {
            Some(load_config(args.config.as_deref(), env)?)
        }
        _ => None,
    };
    let store = house_keys::Store::beside(&db);
    let text = args.command.run(
        &store,
        config.as_ref(),
        &house_keys::process_env(),
        std::io::stdin().lock(),
    )?;
    println!("{text}");
    Ok(true)
}

/// The `lg refine` commands. Each one needs a `[refine]` section.
#[derive(Debug, Subcommand)]
enum RefineCommand {
    /// Ask the model for an overlay from the assistant transcript. The
    /// overlay waits until you activate it.
    Propose,
    /// List the overlay versions. `*` marks the active one.
    List,
    /// Print one overlay version.
    Show {
        /// The version, from `lg refine list`.
        version: u64,
    },
    /// Store an overlay JSON file as a new version. It is not active.
    Put {
        /// The file. `{}` is the config profile with no change.
        file: PathBuf,
    },
    /// Make the assistant run on one version from its next message.
    Activate {
        /// The version, from `lg refine list`.
        version: u64,
        /// The version that must be active now, or `none`. When another
        /// version is active, nothing changes.
        #[arg(long, value_parser = parse_expected)]
        expected: Option<ExpectedActive>,
    },
    /// Activate the version that was active before the current one.
    Rollback,
}

impl RefineCommand {
    /// The request of this command.
    ///
    /// # Errors
    ///
    /// Returns an error when the overlay file of `put` does not read.
    fn request(self) -> Result<Request> {
        Ok(match self {
            Self::Propose => Request::RefinePropose,
            Self::List => Request::Overlays,
            Self::Show { version } => Request::OverlayShow { version },
            Self::Put { file } => Request::OverlayPut {
                overlay: std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?,
            },
            Self::Activate { version, expected } => Request::OverlayActivate { version, expected },
            Self::Rollback => Request::OverlayRollback,
        })
    }
}

/// The `--expected` version of `lg refine activate`: a number, or `none`.
///
/// # Errors
///
/// Returns an error for any other text.
fn parse_expected(text: &str) -> Result<ExpectedActive, String> {
    if text == "none" {
        return Ok(ExpectedActive::None);
    }
    match text.parse::<u64>() {
        Ok(version) if version > 0 => Ok(ExpectedActive::Version(version)),
        _ => Err(String::from("a version from `lg refine list`, or `none`")),
    }
}

/// One patch of the assistant (ADR 0042).
#[derive(Debug, Subcommand)]
enum PatchCommand {
    /// Print the patch whole: its body and its diff.
    Show {
        /// The patch id, from `lg patches`.
        id: i64,
    },
    /// Print the diff, ask, and open the patch as a draft PR.
    Open {
        /// The patch id, from `lg patches`.
        id: i64,
    },
    /// Decline the patch.
    Decline {
        /// The patch id, from `lg patches`.
        id: i64,
        /// Why, for the assistant.
        #[arg(long)]
        reason: Option<String>,
    },
}

/// Runs one `lg patch` command. An open prints the diff and opens only on
/// a `y`, with the digest of the diff that it printed.
async fn patch(socket: &Path, command: PatchCommand) -> Result<bool> {
    let id = match &command {
        PatchCommand::Show { id }
        | PatchCommand::Open { id }
        | PatchCommand::Decline { id, .. } => *id,
    };
    let listing = client::send(socket, &Request::Patches).await?;
    let Response::Patches { patches, .. } = &listing else {
        let (text, _) = client::render(&listing);
        println!("{text}");
        return Ok(false);
    };
    let Some(found) = patches.iter().find(|patch| patch.id == id) else {
        println!("error: there is no patch {id}. Run `lg patches`");
        return Ok(false);
    };
    match command {
        PatchCommand::Show { .. } => {
            println!("{}", client::render_patch(found));
            Ok(true)
        }
        PatchCommand::Open { .. } => {
            println!("{}", client::render_patch(found));
            print!("Open patch {id} as a draft PR? [y/N] ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("patch {id} still waits");
                return Ok(false);
            }
            let request = Request::OpenPatch {
                id,
                digest: found.digest.clone(),
            };
            ask(socket, &request).await
        }
        PatchCommand::Decline { reason, .. } => {
            ask(socket, &Request::DeclinePatch { id, reason }).await
        }
    }
}

/// One saved script of the assistant (ADR 0046).
#[derive(Debug, Subcommand)]
enum ScriptCommand {
    /// Print a draft whole: its description and its code.
    Show {
        /// The draft id, from `lg scripts`.
        id: i64,
    },
    /// Print the draft, ask, and accept it. A tick may then run it.
    Accept {
        /// The draft id, from `lg scripts`.
        id: i64,
    },
    /// Decline the draft.
    Decline {
        /// The draft id, from `lg scripts`.
        id: i64,
    },
    /// Retire an accepted script, so it no longer runs.
    Retire {
        /// The script name, from `lg scripts`.
        name: String,
    },
}

/// Runs one `lg script` command. An accept prints the code and accepts
/// only on a `y`, with the digest of the draft that it printed.
async fn script(socket: &Path, command: ScriptCommand) -> Result<bool> {
    let id = match &command {
        ScriptCommand::Retire { name } => {
            return ask(socket, &Request::RetireScript { name: name.clone() }).await;
        }
        ScriptCommand::Decline { id } => {
            return ask(socket, &Request::DeclineScript { id: *id }).await;
        }
        ScriptCommand::Show { id } | ScriptCommand::Accept { id } => *id,
    };
    let listing = client::send(socket, &Request::Scripts).await?;
    let Response::Scripts { drafts, .. } = &listing else {
        let (text, _) = client::render(&listing);
        println!("{text}");
        return Ok(false);
    };
    let Some(found) = drafts.iter().find(|draft| draft.id == id) else {
        println!("error: no script draft {id} waits. Run `lg scripts`");
        return Ok(false);
    };
    println!("{}", client::render_script_draft(found));
    if matches!(command, ScriptCommand::Show { .. }) {
        return Ok(true);
    }
    print!(
        "Accept script {}? A tick may then run it. [y/N] ",
        found.name
    );
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        println!("draft {id} still waits");
        return Ok(false);
    }
    let request = Request::AcceptScript {
        id,
        digest: found.digest.clone(),
    };
    ask(socket, &request).await
}

/// The models that `lg serve` can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelChoice {
    /// The offline echo model. It needs no key and no network.
    Offline,
    /// Claude over the Messages API. It needs `ANTHROPIC_API_KEY`.
    Claude,
    /// OpenAI, or any server that speaks its API. It needs `OPENAI_API_KEY`
    /// and `--model-name`.
    #[value(name = "openai")]
    OpenAi,
    /// OpenRouter. It needs `OPENROUTER_API_KEY` and `--model-name`.
    #[value(name = "openrouter")]
    OpenRouter,
}

/// The values of `--reasoning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ReasoningChoice {
    /// No reasoning.
    None,
    /// The least that the model offers.
    Minimal,
    /// Little reasoning.
    Low,
    /// The middle.
    Medium,
    /// The most.
    High,
}

#[cfg(feature = "openai")]
impl From<ReasoningChoice> for littleguy_models::Reasoning {
    fn from(choice: ReasoningChoice) -> Self {
        match choice {
            ReasoningChoice::None => Self::None,
            ReasoningChoice::Minimal => Self::Minimal,
            ReasoningChoice::Low => Self::Low,
            ReasoningChoice::Medium => Self::Medium,
            ReasoningChoice::High => Self::High,
        }
    }
}

/// The variable that holds the Claude API key.
#[cfg(feature = "claude")]
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The variable that holds the Discord bot token.
const DISCORD_TOKEN_VAR: &str = "DISCORD_BOT_TOKEN";

/// The variable that holds the Slack bot token (`xoxb-`).
const SLACK_BOT_VAR: &str = "SLACK_BOT_TOKEN";

/// The variable that holds the Slack app-level token (`xapp-`), for Socket
/// Mode.
const SLACK_APP_VAR: &str = "SLACK_APP_TOKEN";

/// The variable that holds the signing secret of the Slack app, for the
/// Events API.
const SLACK_SECRET_VAR: &str = "SLACK_SIGNING_SECRET";

/// The directory of the raw Claude turns, next to the database.
#[cfg(feature = "claude")]
const TURN_DIR: &str = "claude-turns";

#[derive(Debug, Clone, clap::Args)]
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
    /// The model to ask. Claude defaults to `claude-opus-5-5`. OpenAI and
    /// OpenRouter have no default. OpenRouter names take a vendor prefix,
    /// such as `<vendor>/<model>`.
    #[arg(long, env = "LG_MODEL_NAME", alias = "claude-model")]
    model_name: Option<String>,
    /// The model of the reviewer of `[sandbox] approve = "auto"`, on the
    /// provider of `--model`. Defaults to the model of the assistant. A
    /// small, fast model fits: it answers one short judgment per call.
    #[cfg(feature = "auto-approve")]
    #[arg(long, env = "LG_REVIEWER_MODEL_NAME")]
    reviewer_model_name: Option<String>,
    /// The base URL of an OpenAI-compatible server, such as a local one.
    /// Defaults to the provider's own.
    #[arg(long, env = "LG_BASE_URL")]
    base_url: Option<String>,
    /// How hard a reasoning model thinks before it answers, for
    /// `--model openai` and `openrouter`. Hidden reasoning is output, so
    /// less of it answers sooner. Defaults to the model's own.
    #[arg(long, env = "LG_REASONING", value_enum)]
    reasoning: Option<ReasoningChoice>,
    /// The Messages API endpoint. For tests.
    #[arg(long, env = "LG_CLAUDE_ENDPOINT", hide = true)]
    claude_endpoint: Option<String>,
    /// The Discord REST base URL. For tests.
    #[arg(long, env = "LG_DISCORD_API", hide = true)]
    discord_api: Option<String>,
    /// The Discord gateway URL. For tests.
    #[arg(long, env = "LG_DISCORD_GATEWAY", hide = true)]
    discord_gateway: Option<String>,
    /// The Slack Web API base URL. For tests.
    #[arg(long, env = "LG_SLACK_API", hide = true)]
    slack_api: Option<String>,
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

/// The config file that a reload reads: the explicit one, or the default
/// path, even before it exists.
fn config_file(explicit: Option<&Path>, env: &Env) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| env.config().ok())
}

/// The model that `args` names. `key` reads one environment variable.
/// The raw Claude turns live next to `db`, so a restart replays each
/// thinking block.
fn build_model(
    args: &ServeArgs,
    db: &Path,
    key: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn AgentModel>> {
    #[cfg(any(feature = "claude", feature = "openai"))]
    let key_of = |var: &str, model: &str| {
        key(var)
            .filter(|key| !key.trim().is_empty())
            .with_context(|| format!("set {var} to run --model {model}"))
    };
    #[cfg(not(any(feature = "claude", feature = "openai")))]
    let _ = (&key, db);
    #[cfg(not(feature = "claude"))]
    let _ = db;
    // Claude takes its thinking another way, and the offline model has none.
    if args.reasoning.is_some()
        && !matches!(args.model, ModelChoice::OpenAi | ModelChoice::OpenRouter)
    {
        anyhow::bail!("--reasoning works with --model openai and --model openrouter only");
    }
    Ok(match args.model {
        ModelChoice::Offline => Arc::new(
            args.offline_call_log
                .clone()
                .map_or_else(OfflineModel::new, OfflineModel::with_call_log),
        ),
        #[cfg(not(feature = "claude"))]
        ModelChoice::Claude => anyhow::bail!(features::missing("--model claude", "claude")),
        #[cfg(not(feature = "openai"))]
        ModelChoice::OpenAi => anyhow::bail!(features::missing("--model openai", "openai")),
        #[cfg(not(feature = "openai"))]
        ModelChoice::OpenRouter => {
            anyhow::bail!(features::missing("--model openrouter", "openai"))
        }
        #[cfg(feature = "claude")]
        ModelChoice::Claude => {
            let turns = db.parent().unwrap_or_else(|| Path::new(".")).join(TURN_DIR);
            let mut model = ClaudeModel::new(key_of(API_KEY_VAR, "claude")?).turn_dir(turns);
            if let Some(name) = &args.model_name {
                model = model.model(name);
            }
            if let Some(endpoint) = &args.claude_endpoint {
                model = model.endpoint(endpoint);
            }
            Arc::new(model)
        }
        #[cfg(feature = "openai")]
        ModelChoice::OpenAi | ModelChoice::OpenRouter => {
            let (provider, flag) = if args.model == ModelChoice::OpenAi {
                (Provider::OpenAi, "openai")
            } else {
                (Provider::OpenRouter, "openrouter")
            };
            let api_key = key_of(provider.key_var(), flag)?;
            let name = args
                .model_name
                .as_deref()
                .with_context(|| format!("--model {flag} needs --model-name"))?;
            let mut model = OpenAiModel::new(provider, api_key, name);
            if let Some(base_url) = &args.base_url {
                model = model.base_url(base_url);
            }
            if let Some(effort) = args.reasoning {
                model = model.reasoning(effort.into());
            }
            Arc::new(model)
        }
    })
}

/// The model of the reviewer, when `args` names one: the provider of the
/// assistant with another model name (ADR 0037).
#[cfg(feature = "auto-approve")]
fn build_reviewer(
    args: &ServeArgs,
    db: &Path,
    key: impl Fn(&str) -> Option<String>,
) -> Result<Option<Arc<dyn AgentModel>>> {
    let Some(name) = &args.reviewer_model_name else {
        return Ok(None);
    };
    let reviewer = ServeArgs {
        model_name: Some(name.clone()),
        // The reviewer answers one short judgment, so it thinks as its
        // model does by default.
        reasoning: None,
        ..args.clone()
    };
    build_model(&reviewer, db, key)
        .map(Some)
        .context("--reviewer-model-name")
}

/// The Discord link, when `key` holds a bot token.
#[cfg(feature = "discord")]
fn discord_link(
    args: &ServeArgs,
    key: impl Fn(&str) -> Option<String>,
) -> Option<daemon::DiscordLink> {
    let token = key(DISCORD_TOKEN_VAR).filter(|token| !token.trim().is_empty())?;
    let base = args
        .discord_api
        .clone()
        .unwrap_or_else(|| String::from(discord::API_BASE));
    Some(daemon::DiscordLink {
        rest: DiscordRest::new(base, token.clone()),
        token,
        gateway_url: args
            .discord_gateway
            .clone()
            .unwrap_or_else(|| String::from(discord::GATEWAY)),
    })
}

/// The Slack link of `key`. The bot token with the app token is Socket
/// Mode. The bot token with the signing secret is the Events API, which
/// needs the hooks. A part alone is a mistake, so the log says what is
/// missing.
///
/// # Errors
///
/// Returns an error when `key` holds both the app token and the signing
/// secret, since lg cannot tell which mode the owner wants.
#[cfg(feature = "slack")]
fn slack_link(
    args: &ServeArgs,
    key: impl Fn(&str) -> Option<String>,
    hooks: bool,
) -> Result<Option<daemon::SlackLink>> {
    let token = |var| key(var).filter(|token: &String| !token.trim().is_empty());
    let secret = key(SLACK_SECRET_VAR).and_then(littleguy_channels::SigningSecret::new);
    let base = || {
        args.slack_api
            .clone()
            .unwrap_or_else(|| String::from(slack::API_BASE))
    };
    match (token(SLACK_BOT_VAR), token(SLACK_APP_VAR), secret) {
        (_, Some(_), Some(_)) => anyhow::bail!(
            "set {SLACK_APP_VAR} for Socket Mode or {SLACK_SECRET_VAR} for the Events API, not both"
        ),
        (Some(bot), Some(app), None) => Ok(Some(daemon::SlackLink::Socket(SlackApi::new(
            base(),
            bot,
            app,
        )))),
        (Some(bot), None, Some(secret)) if hooks => Ok(Some(daemon::SlackLink::Events {
            api: SlackApi::web_only(base(), bot),
            secret,
        })),
        (Some(_), None, Some(_)) => {
            tracing::warn!(
                "{SLACK_SECRET_VAR} needs a [hooks] section for the Events API: Slack stays off"
            );
            Ok(None)
        }
        (None, None, None) => Ok(None),
        (Some(_), None, None) => {
            tracing::warn!(
                "{SLACK_BOT_VAR} is set without {SLACK_APP_VAR} or {SLACK_SECRET_VAR}: Slack stays off"
            );
            Ok(None)
        }
        (None, _, _) => {
            tracing::warn!("Slack is set up without {SLACK_BOT_VAR}: Slack stays off");
            Ok(None)
        }
    }
}

/// The chat links of `args`. A token of a service that this build leaves
/// out turns nothing on, and the log says so. `hooks` says whether the
/// hooks serve.
///
/// # Errors
///
/// Returns an error when the Slack variables ask for two modes at once.
#[cfg_attr(
    not(feature = "slack"),
    expect(
        clippy::unnecessary_wraps,
        reason = "only the Slack variables can fail, and this build has no Slack"
    )
)]
fn links(
    args: &ServeArgs,
    key: impl Fn(&str) -> Option<String>,
    hooks: bool,
) -> Result<daemon::Links> {
    let unbuilt = [
        (DISCORD_TOKEN_VAR, "discord"),
        (SLACK_BOT_VAR, "slack"),
        (SLACK_APP_VAR, "slack"),
        (SLACK_SECRET_VAR, "slack"),
    ];
    for (var, feature) in unbuilt {
        if !features::has(feature) && key(var).is_some_and(|token| !token.trim().is_empty()) {
            tracing::warn!("{var} is set, but this lg is built without the `{feature}` feature");
        }
    }
    #[cfg(not(any(feature = "discord", feature = "slack")))]
    let _ = args;
    #[cfg(not(feature = "slack"))]
    let _ = hooks;
    Ok(daemon::Links {
        #[cfg(feature = "discord")]
        discord: discord_link(args, &key),
        #[cfg(feature = "slack")]
        slack: slack_link(args, &key, hooks)?,
    })
}

/// The API link on every San Mateo integration of `config`. The `[api]`
/// section picks the ones that get tools at each reload.
#[cfg(feature = "api")]
fn api_link(config: &Config, connections: Option<&oauth::Connections>) -> Option<tools::ApiLink> {
    let (started, section) = connections.zip(config.san_mateo.as_ref())?;
    let keys: Vec<String> = section.integrations.iter().map(|i| i.key.clone()).collect();
    let (proxy, integrations) = started.api(&keys);
    Some((
        Arc::new(proxy) as Arc<dyn littleguy_tools::api::ProviderApi>,
        integrations,
    ))
}

/// Starts San Mateo for the `[san_mateo]` section of `config`, if any.
/// Its ring comes from the environment, and each client secret from
/// `keys`.
#[cfg(feature = "oauth")]
async fn connections(
    config: &Config,
    db: &Path,
    keys: &house_keys::HouseKeys,
) -> Result<Option<oauth::Connections>> {
    let Some(section) = &config.san_mateo else {
        return Ok(None);
    };
    let ring = house_keys::ring(house_keys::process_env().as_ref())?.with_context(|| {
        format!(
            "set {}, 32 random bytes in base64, to keep OAuth connections",
            oauth::KEY_VAR
        )
    })?;
    let started = oauth::Connections::start(
        section,
        config.owner.as_str(),
        db,
        ring,
        |var| keys.get(var),
        oauth::Hooks::default(),
    )
    .await?;
    Ok(Some(started))
}

/// Starts the camel routes of `config` on `mapper`, if `config` has any.
#[cfg(feature = "camel")]
async fn camel(config: &Config, mapper: camel::Mapper, db: &Path) -> Result<Option<camel::Camel>> {
    if config.camel.is_empty() {
        return Ok(None);
    }
    Ok(Some(camel::Camel::start(mapper, db).await?))
}

/// The bearer token of the hooks, from the house key that the config names.
fn hook_token(config: &Config, keys: &house_keys::HouseKeys) -> Result<Option<web::HookToken>> {
    config
        .hooks
        .as_ref()
        .map(|hooks| web::HookToken::new(&hooks.token_env, keys.get(&hooks.token_env)))
        .transpose()
        .map_err(Into::into)
}

/// The daemon with its config file, which it watches and `lg reload`
/// reads, and the skills and scripts folders next to it, unless `[skills]`
/// or `[scripts]` names one.
fn with_config_file(daemon: daemon::Daemon, file: Option<PathBuf>) -> daemon::Daemon {
    let Some(file) = file else {
        return daemon;
    };
    #[cfg(feature = "skills")]
    let skills = file.parent().map(|parent| parent.join("skills"));
    #[cfg(feature = "scripts")]
    let scripts = file.parent().map(|parent| parent.join("scripts"));
    let daemon = daemon.with_config_file(file);
    #[cfg(feature = "skills")]
    let daemon = match skills {
        Some(dir) => daemon.with_skills_dir(dir),
        None => daemon,
    };
    #[cfg(feature = "scripts")]
    let daemon = match scripts {
        Some(dir) => daemon.with_scripts_dir(dir),
        None => daemon,
    };
    daemon
}

/// The Slack hook of the Events API, when `links` holds that mode.
#[cfg(feature = "slack")]
fn slack_hook(
    links: &daemon::Links,
    daemon: &daemon::Daemon,
    handle: &daemon::Handle,
) -> Option<web::SlackHook> {
    match &links.slack {
        Some(daemon::SlackLink::Events { api, secret }) => Some(web::SlackHook::new(
            secret,
            daemon.slack_inbox(api.clone(), handle),
        )),
        _ => None,
    }
}

async fn serve(socket: PathBuf, args: ServeArgs, env: &Env) -> Result<()> {
    let config = load_config(args.config.as_deref(), env)?;
    features::check(&config)?;
    let db = match args.db.clone() {
        Some(db) => db,
        None => env.db()?,
    };
    // The house keys open first: each secret below reads through them.
    let keys = Arc::new(house_keys::HouseKeys::load(&db, house_keys::process_env())?);
    let secret = |var: &str| keys.get(var);
    let model = build_model(&args, &db, secret)?;
    let links = links(&args, secret, config.hooks.is_some())?;
    let uid = rustix::process::getuid().as_raw();
    // San Mateo starts first, because the Google tools send through it.
    #[cfg(feature = "oauth")]
    let connections = connections(&config, &db, &keys).await?;
    let sandbox = config.sandbox.as_ref().map(sandbox::runtime).transpose()?;
    // The tenant database opens before the daemon, which keeps its stores
    // there.
    #[cfg(feature = "fleet")]
    let tenant = match &config.fleet {
        Some(fleet) => Some(fleet::Tenant::open(fleet, &db, &config.owner).await?),
        None => None,
    };
    let mcp = tools::mcp_tools(&config.mcp, keys.lookup()).await;
    let extras = daemon::Extras {
        sandbox,
        #[cfg(feature = "scripts")]
        scripts: scripts::runtime(&config)?,
        mcp,
        #[cfg(feature = "google")]
        google: connections
            .as_ref()
            .zip(config.google.as_ref())
            .map(|(started, google)| {
                Arc::new(started.google(&google.integration))
                    as Arc<dyn littleguy_tools::google::GoogleApi>
            }),
        #[cfg(feature = "api")]
        api: api_link(&config, connections.as_ref()),
        #[cfg(feature = "slack")]
        slack: links.slack.is_some(),
        #[cfg(not(feature = "slack"))]
        slack: false,
        #[cfg(feature = "fleet")]
        tenant: tenant.as_ref().map(|tenant| tenant.path().to_path_buf()),
        #[cfg(not(feature = "fleet"))]
        tenant: None,
        #[cfg(feature = "auto-approve")]
        reviewer: build_reviewer(&args, &db, secret)?,
        keys: Arc::clone(&keys),
    };
    #[cfg(feature = "discord")]
    let discord = links.discord.is_some();
    #[cfg(not(feature = "discord"))]
    let discord = false;
    // The hook token is read first, so a missing one fails before the
    // daemon opens its database.
    let plan = web::Plan::of(&config);
    let hooks = hook_token(&config, &keys)?;
    let daemon = daemon::Daemon::open_with(&db, &config, model, uid, discord, extras)?;
    // The daemon watches its config file, and `lg reload` reads it too.
    let daemon = with_config_file(daemon, config_file(args.config.as_deref(), env));
    #[cfg(feature = "oauth")]
    let daemon = match connections {
        Some(started) => daemon.with_connections(started),
        None => daemon,
    };
    let listener = daemon::bind(&socket).await?;
    #[cfg(feature = "camel")]
    let camel = camel(&config, daemon.camel_mapper(&config), &db).await?;
    #[cfg(not(feature = "camel"))]
    let camel: Option<camel::Camel> = None;
    let (handle, jobs) = daemon::jobs(uid);
    let handle = handle.with_live(daemon.live()).with_reads(daemon.reads());
    let logins = web::Logins::new(web::base_url(plan.bind));
    // A daemon whose pages do not serve refuses `lg ui`.
    let daemon = if plan.pages {
        daemon.with_logins(logins.clone())
    } else {
        daemon
    };
    #[cfg(feature = "slack")]
    let slack = slack_hook(&links, &daemon, &handle);
    let routes = web::Web {
        handle,
        logins,
        streams: web::Streams::default(),
        decisions: web::Decisions::default(),
        hooks,
        #[cfg(feature = "slack")]
        slack,
    };
    web::serve(routes, &plan, move |stop| {
        tokio::spawn(async move {
            let stopped = async move { stop.notified().await };
            daemon::run(daemon, listener, links, camel, jobs, stopped).await
        })
    })
    .await?;
    // The daemon has stopped, so the replicator ships the last writes.
    #[cfg(feature = "fleet")]
    if let Some(tenant) = tenant {
        tenant.close().await;
    }
    // The socket file goes with the daemon. A crash leaves it, and the next
    // start replaces it.
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Sends one request and prints the answer.
async fn ask(socket: &Path, request: &Request) -> Result<bool> {
    let response = client::send(socket, request).await?;
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
        Command::Serve(args) => Box::pin(serve(socket, args, &env)).await.map(|()| true),
        Command::Chat(args) => {
            let request = Request::Chat {
                message_id: args.message_id.unwrap_or_else(client::fresh_message_id),
                text: args.text,
            };
            ask(&socket, &request).await
        }
        Command::Pair { code } => ask(&socket, &Request::Pair { code }).await,
        Command::Pairs => ask(&socket, &Request::Pairs).await,
        Command::Unpair { sender } => ask(&socket, &Request::Unpair { sender }).await,
        Command::Connect { integration } => ask(&socket, &Request::Connect { integration }).await,
        Command::Connections => ask(&socket, &Request::Connections).await,
        Command::Disconnect {
            integration,
            connection,
        } => {
            let request = Request::Disconnect {
                integration,
                connection,
            };
            ask(&socket, &request).await
        }
        Command::Approvals => ask(&socket, &Request::Approvals).await,
        Command::History { id } => {
            let request = id.map_or(Request::Conversations, |id| Request::Conversation { id });
            ask(&socket, &request).await
        }
        Command::Approve { id } => ask(&socket, &Request::Approve { id }).await,
        Command::Deny { id, reason } => ask(&socket, &Request::Deny { id, reason }).await,
        Command::Refine(command) => ask(&socket, &command.request()?).await,
        Command::Patches => ask(&socket, &Request::Patches).await,
        Command::Scripts => ask(&socket, &Request::Scripts).await,
        Command::Script(command) => script(&socket, command).await,
        Command::Patch(command) => patch(&socket, command).await,
        Command::Ui => ask(&socket, &Request::UiLink).await,
        Command::Reload => ask(&socket, &Request::Reload).await,
        Command::New => ask(&socket, &Request::NewConversation).await,
        #[cfg(feature = "sealed-keys")]
        Command::Keys(args) => keys(args, &env),
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
    match Box::pin(run(Cli::parse())).await {
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

    fn refine_request(words: &[&str]) -> Result<Request> {
        match Cli::try_parse_from(words).unwrap().command {
            Command::Refine(command) => command.request(),
            _ => panic!("not refine"),
        }
    }

    #[test]
    fn each_refine_command_sends_its_request() {
        let cases = [
            (vec!["lg", "refine", "propose"], Request::RefinePropose),
            (vec!["lg", "refine", "list"], Request::Overlays),
            (
                vec!["lg", "refine", "show", "3"],
                Request::OverlayShow { version: 3 },
            ),
            (
                vec!["lg", "refine", "activate", "2"],
                Request::OverlayActivate {
                    version: 2,
                    expected: None,
                },
            ),
            (
                vec!["lg", "refine", "activate", "2", "--expected", "1"],
                Request::OverlayActivate {
                    version: 2,
                    expected: Some(ExpectedActive::Version(1)),
                },
            ),
            (
                vec!["lg", "refine", "activate", "2", "--expected", "none"],
                Request::OverlayActivate {
                    version: 2,
                    expected: Some(ExpectedActive::None),
                },
            ),
            (vec!["lg", "refine", "rollback"], Request::OverlayRollback),
        ];
        for (words, request) in cases {
            assert_eq!(refine_request(&words).unwrap(), request, "{words:?}");
        }
        assert!(Cli::try_parse_from(["lg", "refine", "show", "x"]).is_err());
        for bad in ["0", "x", "-1", ""] {
            assert!(
                Cli::try_parse_from(["lg", "refine", "activate", "2", "--expected", bad]).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn refine_put_sends_the_file_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("base.json");
        std::fs::write(&file, "{}").unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(
            refine_request(&["lg", "refine", "put", path]).unwrap(),
            Request::OverlayPut {
                overlay: String::from("{}")
            }
        );
        let missing = dir.path().join("none.json");
        let error = refine_request(&["lg", "refine", "put", missing.to_str().unwrap()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("none.json"), "{error}");
    }

    #[cfg(all(feature = "auto-approve", feature = "openai"))]
    #[test]
    fn the_reviewer_runs_on_the_provider_of_the_assistant() {
        let db = Path::new("/d/lg.db");
        let key = |var: &str| (var == "OPENROUTER_API_KEY").then(|| String::from("k"));
        let plain = serve_args(&[
            "lg",
            "serve",
            "--model",
            "openrouter",
            "--model-name",
            "a/b",
        ]);
        assert!(
            build_reviewer(&plain, db, key).unwrap().is_none(),
            "no flag, no own model"
        );
        let args = serve_args(&[
            "lg",
            "serve",
            "--model",
            "openrouter",
            "--model-name",
            "a/b",
            "--reviewer-model-name",
            "typesafe/jev-router",
        ]);
        let reviewer = build_reviewer(&args, db, key).unwrap().unwrap();
        assert_eq!(reviewer.model_id(), Some("openrouter:typesafe/jev-router"));
        let error = build_reviewer(&args, db, |_| None).err().unwrap();
        assert!(format!("{error:#}").contains("--reviewer-model-name"));
    }

    fn serve_args(words: &[&str]) -> ServeArgs {
        let cli = Cli::try_parse_from(words).unwrap();
        match cli.command {
            Command::Serve(args) => args,
            _ => panic!("not serve"),
        }
    }

    #[cfg(feature = "claude")]
    #[test]
    fn the_claude_model_needs_a_key() {
        let args = serve_args(&["lg", "serve", "--model", "claude"]);
        let db = Path::new("/d/lg.db");
        let error = build_model(&args, db, |_| None).err().unwrap();
        assert!(error.to_string().contains(API_KEY_VAR));
        assert!(build_model(&args, db, |_| Some(String::from("  "))).is_err());
    }

    #[cfg(feature = "claude")]
    #[test]
    fn the_claude_model_takes_its_name_and_endpoint() {
        let args = serve_args(&[
            "lg",
            "serve",
            "--model",
            "claude",
            "--model-name",
            "claude-sonnet-5-5",
            "--claude-endpoint",
            "http://127.0.0.1:1/v1/messages",
        ]);
        let model = build_model(&args, Path::new("/d/lg.db"), |_| Some(String::from("k"))).unwrap();
        assert_eq!(model.model_id(), Some("claude-sonnet-5-5"));
        let args = serve_args(&["lg", "serve", "--model", "claude"]);
        let model = build_model(&args, Path::new("lg.db"), |_| Some(String::from("k"))).unwrap();
        assert_eq!(
            model.model_id(),
            Some("claude-opus-5-5+effort=high+fallbacks=default")
        );
    }

    #[test]
    fn the_old_claude_model_flag_still_works() {
        let args = serve_args(&["lg", "serve", "--claude-model", "x"]);
        assert_eq!(args.model_name.as_deref(), Some("x"));
    }

    #[cfg(feature = "openai")]
    #[test]
    fn openai_and_openrouter_read_their_own_key_and_need_a_name() {
        let only =
            |wanted: &'static str| move |var: &str| (var == wanted).then(|| String::from("k"));
        let db = Path::new("/d/lg.db");
        let args = serve_args(&["lg", "serve", "--model", "openai", "--model-name", "gpt-x"]);
        let model = build_model(&args, db, only("OPENAI_API_KEY")).unwrap();
        assert_eq!(model.model_id(), Some("openai:gpt-x"));
        let error = build_model(&args, db, only("OPENROUTER_API_KEY"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("OPENAI_API_KEY"));

        let args = serve_args(&[
            "lg",
            "serve",
            "--model",
            "openrouter",
            "--model-name",
            "a/b",
            "--base-url",
            "http://127.0.0.1:1/v1",
        ]);
        let model = build_model(&args, db, only("OPENROUTER_API_KEY")).unwrap();
        assert_eq!(model.model_id(), Some("openrouter:a/b"));

        let nameless = serve_args(&["lg", "serve", "--model", "openrouter"]);
        let error = build_model(&nameless, db, only("OPENROUTER_API_KEY"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("--model-name"));
    }

    #[test]
    fn reasoning_is_for_the_openai_adapter_only() {
        let args = serve_args(&["lg", "serve", "--reasoning", "low"]);
        assert_eq!(args.reasoning, Some(ReasoningChoice::Low));
        let error = build_model(&args, Path::new("/d/lg.db"), |_| None)
            .err()
            .unwrap();
        assert!(error.to_string().contains("--model openai"), "{error}");
        assert!(Cli::try_parse_from(["lg", "serve", "--reasoning", "lots"]).is_err());
        #[cfg(feature = "openai")]
        {
            let args = serve_args(&[
                "lg",
                "serve",
                "--model",
                "openrouter",
                "--model-name",
                "a/b",
                "--reasoning",
                "none",
            ]);
            let key = |var: &str| (var == "OPENROUTER_API_KEY").then(|| String::from("k"));
            assert!(build_model(&args, Path::new("/d/lg.db"), key).is_ok());
        }
    }

    #[cfg(feature = "slack")]
    #[test]
    fn slack_runs_over_a_socket_or_over_the_hooks() {
        use daemon::SlackLink;
        let args = serve_args(&["lg", "serve"]);
        let with = |vars: &'static [&'static str]| {
            move |var: &str| vars.contains(&var).then(|| String::from("t"))
        };
        let link = |vars, hooks| slack_link(&args, with(vars), hooks).unwrap();
        assert!(link(&[], true).is_none());
        assert!(link(&[SLACK_BOT_VAR], true).is_none());
        assert!(link(&[SLACK_APP_VAR], true).is_none());
        assert!(link(&[SLACK_SECRET_VAR], true).is_none());
        assert!(slack_link(&args, with(&[SLACK_APP_VAR, SLACK_SECRET_VAR]), true).is_err());
        assert!(
            slack_link(&args, |_| Some(String::from(" ")), true)
                .unwrap()
                .is_none()
        );

        let socket = link(&[SLACK_BOT_VAR, SLACK_APP_VAR], false).unwrap();
        let SlackLink::Socket(api) = socket else {
            panic!("not Socket Mode");
        };
        assert!(format!("{api:?}").contains(slack::API_BASE));

        // The Events API needs the hooks, since Slack calls them.
        assert!(link(&[SLACK_BOT_VAR, SLACK_SECRET_VAR], false).is_none());
        let events = link(&[SLACK_BOT_VAR, SLACK_SECRET_VAR], true).unwrap();
        assert!(matches!(events, SlackLink::Events { .. }));
        assert!(!format!("{events:?}").contains("\"t\""));

        let both = slack_link(
            &args,
            with(&[SLACK_BOT_VAR, SLACK_APP_VAR, SLACK_SECRET_VAR]),
            true,
        );
        let error = both.err().unwrap().to_string();
        assert!(error.contains("not both"), "{error}");

        let args = serve_args(&["lg", "serve", "--slack-api", "http://127.0.0.1:1/api"]);
        let socket = slack_link(&args, with(&[SLACK_BOT_VAR, SLACK_APP_VAR]), false)
            .unwrap()
            .unwrap();
        assert!(format!("{socket:?}").contains("127.0.0.1:1"));
    }

    #[cfg(feature = "discord")]
    #[test]
    fn discord_runs_only_with_a_token() {
        let args = serve_args(&["lg", "serve"]);
        assert!(discord_link(&args, |_| None).is_none());
        assert!(discord_link(&args, |_| Some(String::from(" "))).is_none());
        let link = discord_link(&args, |var| {
            (var == DISCORD_TOKEN_VAR).then(|| String::from("secret-token"))
        })
        .unwrap();
        assert_eq!(link.gateway_url, discord::GATEWAY);
        assert!(!format!("{link:?}").contains("secret-token"));
        let args = serve_args(&[
            "lg",
            "serve",
            "--discord-api",
            "http://127.0.0.1:1/api",
            "--discord-gateway",
            "ws://127.0.0.1:1",
        ]);
        let link = discord_link(&args, |_| Some(String::from("t"))).unwrap();
        assert_eq!(link.gateway_url, "ws://127.0.0.1:1");
    }

    #[test]
    fn a_model_of_a_missing_feature_names_the_feature() {
        let db = Path::new("/d/lg.db");
        let key = |_: &str| Some(String::from("k"));
        for (model, feature) in [
            ("claude", "claude"),
            ("openai", "openai"),
            ("openrouter", "openai"),
        ] {
            let args = serve_args(&["lg", "serve", "--model", model, "--model-name", "m"]);
            let built = build_model(&args, db, key);
            if features::has(feature) {
                assert!(built.is_ok(), "{model}");
            } else {
                let error = built.err().unwrap().to_string();
                assert!(error.contains(&format!("`{feature}` feature")), "{error}");
            }
        }
    }

    #[test]
    fn a_token_of_a_missing_service_turns_nothing_on() {
        let args = serve_args(&["lg", "serve"]);
        let links = links(
            &args,
            |var| (var != SLACK_SECRET_VAR).then(|| String::from("t")),
            false,
        )
        .unwrap();
        #[cfg(feature = "discord")]
        assert!(links.discord.is_some());
        #[cfg(feature = "slack")]
        assert!(links.slack.is_some());
        let _ = links;
    }

    #[test]
    fn the_cli_parses_the_pairing_commands() {
        let cli = Cli::try_parse_from(["lg", "pair", "ABCD-1234"]).unwrap();
        assert!(matches!(cli.command, Command::Pair { ref code } if code == "ABCD-1234"));
        let cli = Cli::try_parse_from(["lg", "pairs"]).unwrap();
        assert!(matches!(cli.command, Command::Pairs));
        let cli = Cli::try_parse_from(["lg", "connect", "google"]).unwrap();
        assert!(
            matches!(cli.command, Command::Connect { ref integration } if integration == "google")
        );
        let cli = Cli::try_parse_from(["lg", "connections"]).unwrap();
        assert!(matches!(cli.command, Command::Connections));
        let cli = Cli::try_parse_from(["lg", "disconnect", "google", "c-1"]).unwrap();
        assert!(
            matches!(cli.command, Command::Disconnect { ref connection, .. } if connection == "c-1")
        );
        let cli = Cli::try_parse_from(["lg", "unpair", "discord:1"]).unwrap();
        assert!(matches!(cli.command, Command::Unpair { ref sender } if sender == "discord:1"));
    }

    #[test]
    fn the_offline_model_is_the_default() {
        let args = serve_args(&["lg", "serve"]);
        let model = build_model(&args, Path::new("/d/lg.db"), |_| None).unwrap();
        assert_eq!(model.model_id(), Some("littleguy-offline-1"));
    }

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
