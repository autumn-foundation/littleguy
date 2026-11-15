//! The owner's side of skills, and the sync of the skills folder (ADR 0024).
//!
//! The `SKILL.md` files of the skills folder are the skills. The daemon puts
//! them in the profile overlay whenever the folder changes, so the running
//! assistant lists them in its prompt and loads them with `load_skill`. The
//! overlay is read at each op, so no restart is needed (autumn-harvest
//! #2658 does not bind it). The assistant drafts skills, and only the owner
//! turns a draft into a file.
//!
//! The module comes with the `skills` feature, which needs `refine`.

use std::path::PathBuf;

use anyhow::Result;

use super::Daemon;
use crate::protocol::{InvalidSkill, Request, Response, SkillDraftEntry, SkillEntry};
use crate::skills;

/// The author of a skill that the assistant drafted.
const ASSISTANT_AUTHOR: &str = "littleguy";

/// Why skills are off in this daemon, or `None` when they work.
fn off_reason(daemon: &Daemon) -> Option<String> {
    if daemon.skills_dir().is_none() {
        return Some(String::from(
            "skills need a folder: set `dir` under [skills]",
        ));
    }
    daemon.refine.is_none().then(|| {
        String::from(
            "skills need a [refine] section: they reach the assistant through its profile overlay",
        )
    })
}

impl Daemon {
    /// Sets the skills folder that a config with no `[skills] dir` uses:
    /// `skills/` next to the config file.
    #[must_use]
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills_default = Some(dir);
        self.skills_stamp = None;
        self
    }

    /// The skills folder of this daemon.
    pub(super) fn skills_dir(&self) -> Option<PathBuf> {
        self.kit
            .config
            .skills
            .dir
            .clone()
            .or_else(|| self.skills_default.clone())
    }

    /// Puts the skills of the folder in the overlay, when the folder changed
    /// since the last sync. A failure goes to the log once: the next change
    /// of the folder tries again.
    pub(super) async fn sync_skills(&mut self) {
        {
            let (Some(dir), Some(refine)) = (self.skills_dir(), self.refine.as_deref()) else {
                return;
            };
            let stamp = skills::stamp(&dir);
            if self.skills_stamp.as_ref() == Some(&stamp) {
                return;
            }
            match skills::load(&dir) {
                Ok(found) => {
                    for file in &found.invalid {
                        tracing::warn!(path = %file.path.display(), reason = %file.reason, "a skill does not load");
                    }
                    match refine.set_skills(found.texts()).await {
                        Ok(Some(version)) => {
                            tracing::info!(
                                version,
                                skills = found.skills.len(),
                                "the skills changed"
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(error = %format!("{error:#}"), "the skills did not reach the overlay");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "the skills folder does not read");
                }
            }
            self.skills_stamp = Some(stamp);
        }
    }

    /// The listing of the skills and the drafts.
    fn skills_listing(&self) -> Result<Response> {
        let Some(dir) = self.skills_dir() else {
            return Ok(Response::Off {
                message: String::from("skills need a folder: set `dir` under [skills]"),
            });
        };
        let found = skills::load(&dir)?;
        let drafts = match self.kit.stores.as_ref() {
            Some(stores) => stores.skill_drafts()?,
            None => Vec::new(),
        };
        let path = |path: &std::path::Path| path.display().to_string();
        Ok(Response::Skills {
            drafts: drafts
                .into_iter()
                .map(|draft| SkillDraftEntry {
                    revises: found.skills.iter().any(|skill| skill.name == draft.name),
                    id: draft.id,
                    name: draft.name,
                    description: draft.description,
                    body: draft.body,
                    created_ms: draft.created_ms,
                })
                .collect(),
            skills: found
                .skills
                .into_iter()
                .map(|skill| SkillEntry {
                    path: path(&skill.path),
                    name: skill.name,
                    description: skill.description,
                    body: skill.body,
                    author: skill.author,
                })
                .collect(),
            invalid: found
                .invalid
                .into_iter()
                .map(|file| InvalidSkill {
                    path: path(&file.path),
                    reason: file.reason,
                })
                .collect(),
        })
    }

    /// Handles one skills request of the owner. Each change answers with the
    /// listing after it, and syncs the overlay at once.
    pub(super) async fn skills_request(&mut self, request: Request) -> Result<Response> {
        if let Some(message) = off_reason(self) {
            return Ok(if matches!(request, Request::Skills) {
                Response::Off { message }
            } else {
                Response::Error { message }
            });
        }
        let Some(dir) = self.skills_dir() else {
            return self.skills_listing();
        };
        let refused = |message: String| Ok(Response::Error { message });
        match request {
            Request::AcceptSkill { draft } => {
                let found = match self.kit.stores.as_ref() {
                    Some(stores) => stores.skill_draft(draft)?,
                    None => None,
                };
                let Some(found) = found else {
                    return refused(format!("{draft} is not a draft that waits"));
                };
                skills::write(
                    &dir,
                    &found.name,
                    &found.description,
                    &found.body,
                    ASSISTANT_AUTHOR,
                )?;
                if let Some(stores) = self.kit.stores.as_ref() {
                    stores.close_skill_draft(draft, true)?;
                }
                tracing::info!(skill = %found.name, "the owner accepted a skill");
            }
            Request::DeclineSkill { draft } => {
                let closed = match self.kit.stores.as_ref() {
                    Some(stores) => stores.close_skill_draft(draft, false)?,
                    None => false,
                };
                if !closed {
                    return refused(format!("{draft} is not a draft that waits"));
                }
            }
            Request::RetireSkill { name } => {
                if !skills::retire(&dir, &name)? {
                    return refused(format!("no skill is named {name}"));
                }
                tracing::info!(skill = %name, "the owner retired a skill");
            }
            _ => {}
        }
        self.sync_skills().await;
        self.skills_listing()
    }
}
