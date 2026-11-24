//! Saved scripts: programs that the owner blessed, and a tick can run
//! (ADR 0046).
//!
//! A tick cannot call `run_code`, and that is correct. It also could not
//! run a program that the owner already read, so each tick wrote the same
//! walk of a repository again from nothing. Now the assistant proposes a
//! script with `propose_script`. The owner accepts it on the Scripts tab.
//! The daemon writes it to the scripts folder and records its SHA-256 in
//! `memory.db`. `run_script` then runs it in the sandbox, on a tick too.
//!
//! `run_script` is `ReadOnly`, so a tick may call it. So a blessed script
//! keeps its side effects where a `write_file` call loses them. ADR 0021
//! now reads: a tick changes nothing except through a blessed script. The
//! run log on the Scripts tab shows each run with its arguments.
//!
//! Three rules hold the tool:
//!
//! - It runs only a name that the owner accepted.
//! - It hashes the bytes that it sends to the sandbox, and refuses them
//!   when the hash differs from the recorded one. The pin guards drift
//!   between acceptance and a run. It does not guard a hostile tick, since
//!   code in the sandbox can write `memory.db` and the folder.
//! - The arguments reach the program as argv and stdin only. No shell
//!   string is built from them, so web text in an argument stays data.
//!
//! The `scripts` feature builds the module. It needs `sandbox`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use autumn_harvest::sandbox::{
    IDEMPOTENCY_KEY_ENV, NetworkPolicy, RunOptions, RunStatus, SandboxRequest, SandboxRuntime,
};
use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::sandbox::{model_view, tool_error};
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolContext, ToolEffect};
use jiff::Timestamp;
use littleguy_core::Config;
use littleguy_core::script_name::{MAX_SCRIPT_NAME, is_script_name};
use littleguy_stores::SqliteStores;
use littleguy_stores::scripts::{NewScriptRun, Script, ScriptDraft};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::protocol::{Response, ScriptDraftEntry, ScriptEntry, ScriptRunEntry};

/// The folder that holds retired scripts, inside the scripts folder.
pub const RETIRED: &str = ".retired";

/// The most runs that the Scripts tab shows.
pub const LISTED_RUNS: u32 = 50;

/// The scripts folder of the config that runs. The daemon sets it at each
/// assembly, so a clone in the tools reads the current folder.
#[derive(Debug, Clone, Default)]
pub struct Dir(Arc<Mutex<Option<PathBuf>>>);

impl Dir {
    /// Sets the folder.
    pub fn set(&self, dir: Option<PathBuf>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = dir;
    }

    /// The folder, if any.
    #[must_use]
    pub fn get(&self) -> Option<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The runtime of the scripts: the backend of `[sandbox]` with the network
/// of `[scripts]`. `None` without a `[scripts]` section.
///
/// # Errors
///
/// Returns an error when the backend does not start.
pub fn runtime(config: &Config) -> Result<Option<Arc<SandboxRuntime>>> {
    match (&config.scripts, &config.sandbox) {
        (Some(scripts), Some(sandbox)) => {
            Ok(Some(crate::sandbox::runtime(&scripts.sandbox(sandbox))?))
        }
        _ => Ok(None),
    }
}

/// The SHA-256 of `bytes`, in lower-case hex.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The digest of a draft: the SHA-256 of its name, runtime, description
/// and code, each with its length first. The owner accepts a draft only
/// with the digest of what the owner read. Code in the sandbox can write
/// `memory.db`, so a draft can change between a read and a press.
#[must_use]
pub fn digest(draft: &ScriptDraft) -> String {
    let mut hash = Sha256::new();
    for part in [&draft.name, &draft.runtime, &draft.description, &draft.code] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hex(&hash.finalize())
}

/// The outcome of a run, for the log: `exited 0`, `signal 9` or
/// `timed out`.
fn outcome_of(status: RunStatus) -> String {
    match status {
        RunStatus::Exited { code } => format!("exited {code}"),
        RunStatus::Signaled { signal } => format!("signal {signal}"),
        RunStatus::TimedOut => String::from("timed out"),
    }
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// The key of one tool call. A retried call has the same key.
fn call_key(ctx: &ToolContext) -> String {
    format!("{}:{}", ctx.run_id.as_str(), ctx.call_id)
}

/// The saved scripts: the store, the sandbox and the folder.
#[derive(Debug)]
pub struct Scripts {
    /// The drafts, the accepted scripts and the run log.
    pub stores: Arc<SqliteStores>,
    /// The sandbox that runs a script.
    pub runtime: Arc<SandboxRuntime>,
    /// The scripts folder.
    pub dir: Dir,
}

impl Scripts {
    /// The scripts folder, or why there is none.
    fn folder(&self) -> Result<PathBuf, String> {
        self.dir.get().ok_or_else(|| {
            String::from(
                "scripts need a folder: set `dir` under [scripts], or run with a config file",
            )
        })
    }

    /// Whether a script may use the network.
    #[must_use]
    pub fn network(&self) -> bool {
        self.runtime.policy().network == NetworkPolicy::Allow
    }

    /// The runtimes of the sandbox, by name.
    fn runtimes(&self) -> Vec<String> {
        self.runtime.policy().runtimes.keys().cloned().collect()
    }

    /// The file name of a script `name` on `runtime`: the name and the
    /// extension of the code file of the runtime, such as `walk.py`.
    fn file_name(&self, name: &str, runtime: &str) -> String {
        let extension = self
            .runtime
            .policy()
            .runtimes
            .get(runtime)
            .and_then(|spec| Path::new(&spec.file_name).extension())
            .and_then(|extension| extension.to_str())
            .map(str::to_owned);
        extension.map_or_else(|| name.to_owned(), |ext| format!("{name}.{ext}"))
    }

    /// Stores a draft for the call `key`, after the checks that the store
    /// does not make. Returns its id.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for a bad name, an unknown runtime or an
    /// empty description, and the errors of the store.
    pub fn propose(&self, key: &str, draft: &ScriptDraft) -> Result<i64, AgentError> {
        let name = draft.name.trim();
        if !is_script_name(name) {
            return Err(refuse(format!(
                "`{name}` is not a script name: use 1 to {MAX_SCRIPT_NAME} lower-case letters, \
                 digits and `-`, not starting with `-`"
            )));
        }
        let runtimes = self.runtimes();
        if !runtimes.iter().any(|known| known == draft.runtime.trim()) {
            return Err(refuse(format!(
                "`{}` is not a runtime of the sandbox. Use one of: {}",
                draft.runtime.trim(),
                runtimes.join(", ")
            )));
        }
        if draft.description.trim().is_empty() {
            return Err(refuse(
                "say what the script does, and what its arguments and stdin are",
            ));
        }
        self.stores.add_script_draft(key, draft, now_ms())
    }

    /// Accepts the waiting draft `id`, when `digest` is its digest. Writes
    /// the code to the folder, moves an older file of the name to the
    /// retired folder, and records the hash. Returns the name.
    ///
    /// # Errors
    ///
    /// Returns a message when the draft does not wait, the digest differs,
    /// or the folder or the store fails.
    pub fn accept(&self, id: i64, digest: &str) -> Result<String, String> {
        let dir = self.folder()?;
        let draft = self
            .stores
            .script_draft(id)
            .map_err(|error| error.message().to_owned())?
            .ok_or_else(|| format!("{id} is not a script draft that waits"))?;
        if self::digest(&draft) != digest {
            return Err(format!(
                "draft {id} changed since you read it. Read it again before you accept it"
            ));
        }
        // The store checked neither, and the row may have changed since.
        if !is_script_name(&draft.name) {
            return Err(format!("`{}` is not a script name", draft.name));
        }
        let file = self.file_name(&draft.name, &draft.runtime);
        let fail = |error: std::io::Error| format!("the scripts folder fails: {error}");
        std::fs::create_dir_all(&dir).map_err(fail)?;
        retire_files(&dir, &draft.name).map_err(fail)?;
        let path = dir.join(&file);
        let staged = dir.join(format!(".{file}.new"));
        std::fs::write(&staged, draft.code.as_bytes()).map_err(fail)?;
        std::fs::rename(&staged, &path).map_err(fail)?;
        let script = Script {
            name: draft.name.clone(),
            runtime: draft.runtime.clone(),
            description: draft.description.clone(),
            file,
            sha256: sha256(draft.code.as_bytes()),
            accepted_ms: now_ms(),
        };
        let accepted = self
            .stores
            .accept_script_draft(id, &script)
            .map_err(|error| error.message().to_owned())?;
        if !accepted {
            return Err(format!("{id} is not a script draft that waits"));
        }
        tracing::info!(script = %script.name, sha256 = %script.sha256, "the owner accepted a script");
        Ok(script.name)
    }

    /// Declines the waiting draft `id`.
    ///
    /// # Errors
    ///
    /// Returns a message when the draft does not wait or the store fails.
    pub fn decline(&self, id: i64) -> Result<(), String> {
        match self.stores.decline_script_draft(id) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!("{id} is not a script draft that waits")),
            Err(error) => Err(error.message().to_owned()),
        }
    }

    /// Retires the accepted script `name`: `run_script` refuses it, and its
    /// file moves to the retired folder. Nothing is deleted.
    ///
    /// # Errors
    ///
    /// Returns a message when no script has the name, or the folder or the
    /// store fails.
    pub fn retire(&self, name: &str) -> Result<(), String> {
        let dir = self.folder()?;
        if !self
            .stores
            .retire_script(name)
            .map_err(|error| error.message().to_owned())?
        {
            return Err(format!("no accepted script is named {name}"));
        }
        if is_script_name(name) {
            retire_files(&dir, name)
                .map_err(|error| format!("the scripts folder fails: {error}"))?;
        }
        tracing::info!(script = %name, "the owner retired a script");
        Ok(())
    }

    /// The owner's listing: the drafts, the accepted scripts with their
    /// drift, and the newest runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn listing(&self) -> Result<Response, AgentError> {
        let dir = self.dir.get();
        let scripts = self.stores.scripts()?;
        let drafts = self
            .stores
            .script_drafts()?
            .into_iter()
            .map(|draft| ScriptDraftEntry {
                digest: digest(&draft),
                revises: scripts.iter().any(|script| script.name == draft.name),
                id: draft.id,
                name: draft.name,
                runtime: draft.runtime,
                description: draft.description,
                code: draft.code,
                created_ms: draft.created_ms,
            })
            .collect();
        let scripts = scripts
            .into_iter()
            .map(|script| {
                let path = dir.as_ref().map(|dir| dir.join(&script.file));
                let code = path
                    .as_ref()
                    .and_then(|path| std::fs::read_to_string(path).ok());
                ScriptEntry {
                    drifted: code
                        .as_ref()
                        .is_none_or(|code| sha256(code.as_bytes()) != script.sha256),
                    path: path.map_or_else(|| script.file.clone(), |p| p.display().to_string()),
                    code,
                    name: script.name,
                    runtime: script.runtime,
                    description: script.description,
                    sha256: script.sha256,
                    accepted_ms: script.accepted_ms,
                }
            })
            .collect();
        let runs = self
            .stores
            .script_runs(LISTED_RUNS)?
            .into_iter()
            .map(|run| ScriptRunEntry {
                name: run.name,
                args: run.args,
                stdin: run.stdin,
                outcome: run.outcome,
                duration_ms: run.duration_ms,
                at_ms: run.at_ms,
            })
            .collect();
        Ok(Response::Scripts {
            dir: dir.map(|dir| dir.display().to_string()).unwrap_or_default(),
            network: self.network(),
            drafts,
            scripts,
            runs,
        })
    }

    /// Runs the accepted script `name` with `args` as argv and `stdin`.
    /// Each run, and each refusal of an accepted name, goes in the log.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for a name that was not accepted, a file
    /// that differs from its hash, and the errors of the sandbox.
    pub async fn run(
        &self,
        ctx: &ToolContext,
        name: &str,
        args: Vec<String>,
        stdin: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, AgentError> {
        let Some(script) = self.stores.script(name)? else {
            let accepted: Vec<String> = self
                .stores
                .scripts()?
                .into_iter()
                .map(|script| script.name)
                .collect();
            return Err(refuse(format!(
                "`{name}` is not a script that the owner accepted. Accepted: {}. Propose one \
                 with propose_script",
                if accepted.is_empty() {
                    String::from("none")
                } else {
                    accepted.join(", ")
                }
            )));
        };
        let key = call_key(ctx);
        let log = |outcome: &str, duration_ms: i64| {
            self.stores.record_script_run(&NewScriptRun {
                key: &key,
                name,
                args: &args,
                stdin: stdin.as_deref(),
                run_id: ctx.run_id.as_str(),
                outcome,
                duration_ms,
                at_ms: now_ms(),
            })
        };
        let code = match self.pinned_code(&script) {
            Ok(code) => code,
            Err(why) => {
                log(&format!("refused: {why}"), 0)?;
                return Err(refuse(format!(
                    "`{name}` does not run: {why}. Ask the owner to accept it again"
                )));
            }
        };
        let mut request = SandboxRequest::new(&script.runtime, code);
        request.args.clone_from(&args);
        request.stdin.clone_from(&stdin);
        request.limits.timeout_ms = timeout_ms;
        let mut engine_env = BTreeMap::new();
        engine_env.insert(
            IDEMPOTENCY_KEY_ENV.to_owned(),
            format!("{}:{}:{}", ctx.run_id.as_str(), ctx.step, ctx.call_id),
        );
        let options = RunOptions {
            engine_env,
            time_left: None,
        };
        let ran = self
            .runtime
            .run_with(
                &request,
                options,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await;
        match ran {
            Ok(outcome) => {
                let duration = i64::try_from(outcome.duration_ms).unwrap_or(i64::MAX);
                log(&outcome_of(outcome.status), duration)?;
                let mut view = model_view(&outcome);
                view["script"] = json!(name);
                Ok(view)
            }
            Err(error) => {
                log(&format!("failed: {}", error.message()), 0)?;
                Err(tool_error(&error))
            }
        }
    }

    /// The code of `script`, when its file has the recorded hash. The hash
    /// covers the very bytes that go to the sandbox, so no change can land
    /// between the check and the run.
    fn pinned_code(&self, script: &Script) -> Result<String, String> {
        let dir = self.folder()?;
        let bytes = std::fs::read(dir.join(&script.file))
            .map_err(|error| format!("its file does not read ({error})"))?;
        if sha256(&bytes) != script.sha256 {
            return Err(String::from("its file changed since the owner accepted it"));
        }
        String::from_utf8(bytes).map_err(|_| String::from("its file is not UTF-8 text"))
    }
}

/// Moves each file of the script `name` in `dir` to the retired folder,
/// with a time in its name. A file of the name is `name` or `name.<ext>`.
fn retire_files(dir: &Path, name: &str) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%S%3f").to_string();
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let of_name = file == name
            || file
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|ext| !ext.is_empty() && !ext.contains('.'));
        if !of_name || !entry.file_type()?.is_file() {
            continue;
        }
        let retired = dir.join(RETIRED);
        std::fs::create_dir_all(&retired)?;
        let base = format!("{stamp}-{file}");
        let target = std::iter::once(base.clone())
            .chain((2..=u32::MAX).map(|n| format!("{n}-{base}")))
            .map(|name| retired.join(name))
            .find(|target| !target.exists())
            .unwrap_or_else(|| retired.join(base));
        std::fs::rename(entry.path(), target)?;
    }
    Ok(())
}

/// The text field `field` of `input`.
fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str, AgentError> {
    input[field]
        .as_str()
        .ok_or_else(|| refuse(format!("`{field}` must be text")))
}

/// The arguments of a `run_script` call: a list of strings.
fn args_of(input: &Value) -> Result<Vec<String>, AgentError> {
    match &input["args"] {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| refuse("each item of `args` must be text"))
            })
            .collect(),
        _ => Err(refuse("`args` must be a list of text")),
    }
}

/// The tools of the saved scripts over `scripts`.
#[must_use]
pub fn tools(scripts: &Arc<Scripts>) -> Vec<Arc<dyn Tool>> {
    vec![
        propose_script(scripts),
        list_scripts(scripts),
        read_script(scripts),
        run_script(scripts),
    ]
}

/// `propose_script`: a draft that the owner accepts or declines.
fn propose_script(scripts: &Arc<Scripts>) -> Arc<dyn Tool> {
    let propose = Arc::clone(scripts);
    let runtimes = scripts.runtimes();
    FnTool::with_context(
        "propose_script",
        "Propose a script: a program that you will run again with run_script, on a \
         heartbeat too, where run_code is refused. Propose one when you do the same work on \
         many ticks, such as a walk of a remote repository. It does nothing until the owner \
         accepts it on the Scripts tab; tell them that it waits. A draft with the name of a \
         waiting draft or of an accepted script revises it. The script gets its arguments as \
         argv and its input as stdin. Never build a shell command from an argument.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Lower-case letters, digits and -, such as walk-repo."},
                "runtime": {"type": "string", "enum": runtimes, "description": "The sandbox runtime."},
                "description": {"type": "string", "description": "What it does, its arguments and its stdin, and what it prints."},
                "code": {"type": "string", "description": "The whole program."},
            },
            "required": ["name", "runtime", "description", "code"],
        }),
        move |input, ctx| {
            let scripts = Arc::clone(&propose);
            async move {
                let draft = ScriptDraft {
                    id: 0,
                    name: text(&input, "name")?.to_owned(),
                    runtime: text(&input, "runtime")?.to_owned(),
                    description: text(&input, "description")?.to_owned(),
                    code: text(&input, "code")?.to_owned(),
                    created_ms: 0,
                };
                let id = scripts.propose(&call_key(&ctx), &draft)?;
                Ok(json!({
                    "script_draft": id,
                    "name": draft.name.trim(),
                    "waits": "for the owner, on the Scripts tab",
                }))
            }
        },
    )
    .effect(ToolEffect::Internal)
    .shared()
}

/// `list_scripts`: the accepted scripts and the drafts, without code.
fn list_scripts(scripts: &Arc<Scripts>) -> Arc<dyn Tool> {
    let list = Arc::clone(scripts);
    FnTool::new(
        "list_scripts",
        "List the scripts that you can run with run_script, with what each one does, and your \
         drafts that wait for the owner.",
        json!({"type": "object", "properties": {}}),
        move |_input| {
            let scripts = Arc::clone(&list);
            async move {
                let accepted: Vec<Value> = scripts
                    .stores
                    .scripts()?
                    .into_iter()
                    .map(|script| {
                        json!({
                            "name": script.name,
                            "runtime": script.runtime,
                            "description": script.description,
                        })
                    })
                    .collect();
                let drafts: Vec<Value> = scripts
                    .stores
                    .script_drafts()?
                    .into_iter()
                    .map(|draft| json!({"id": draft.id, "name": draft.name}))
                    .collect();
                Ok(json!({
                    "scripts": accepted,
                    "script_drafts": drafts,
                    "network": scripts.network(),
                }))
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `read_script`: the code of an accepted script or of a waiting draft.
fn read_script(scripts: &Arc<Scripts>) -> Arc<dyn Tool> {
    let read = Arc::clone(scripts);
    FnTool::new(
        "read_script",
        "Read the code of a script by name: the accepted one, and your draft of that name if \
         one waits. Read it before you revise it, since a draft replaces the whole script.",
        json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        }),
        move |input| {
            let scripts = Arc::clone(&read);
            async move {
                let name = text(&input, "name")?.trim();
                let accepted = scripts.stores.script(name)?;
                let draft = scripts
                    .stores
                    .script_drafts()?
                    .into_iter()
                    .find(|draft| draft.name == name);
                if accepted.is_none() && draft.is_none() {
                    return Err(refuse(format!("no script or draft is named `{name}`")));
                }
                let accepted = accepted.map(|script| {
                    json!({
                        "runtime": script.runtime,
                        "description": script.description,
                        "code": scripts.pinned_code(&script).unwrap_or_else(|why| format!("<{why}>")),
                    })
                });
                let draft = draft.map(|draft| {
                    json!({
                        "id": draft.id,
                        "runtime": draft.runtime,
                        "description": draft.description,
                        "code": draft.code,
                    })
                });
                Ok(json!({"name": name, "accepted": accepted, "draft": draft}))
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `run_script`: runs an accepted script in the sandbox. It is `ReadOnly`
/// so that a tick may call it (ADR 0046). Its output may hold outside
/// text, so it carries `untrusted_input`.
fn run_script(scripts: &Arc<Scripts>) -> Arc<dyn Tool> {
    let run = Arc::clone(scripts);
    let network = if scripts.network() {
        "Scripts have network access."
    } else {
        "Scripts have no network."
    };
    FnTool::with_context(
        "run_script",
        format!(
            "Run a script that the owner accepted, by name, and get its exit status, stdout and \
             stderr. It works on a heartbeat too. Give its arguments in `args`: each one reaches \
             the program as one argv entry, never through a shell. Give input in `stdin`. \
             list_scripts names the scripts. {network}"
        ),
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "An accepted script."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "The argv after the program, one entry each."},
                "stdin": {"type": "string", "description": "Text for stdin."},
                "timeout_ms": {"type": "integer", "minimum": 1, "description": "A wall clock limit, up to the sandbox maximum."},
            },
            "required": ["name"],
        }),
        move |input, ctx| {
            let scripts = Arc::clone(&run);
            async move {
                let name = text(&input, "name")?.trim().to_owned();
                let args = args_of(&input)?;
                let stdin = match &input["stdin"] {
                    Value::Null => None,
                    Value::String(stdin) => Some(stdin.clone()),
                    _ => return Err(refuse("`stdin` must be text")),
                };
                let timeout_ms = input["timeout_ms"].as_u64();
                scripts.run(&ctx, &name, args, stdin, timeout_ms).await
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;
