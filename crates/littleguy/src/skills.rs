//! Skills as `SKILL.md` files, as Claude Code and Codex keep them (ADR 0024).
//!
//! Each skill is a folder in the skills folder: `<dir>/<name>/SKILL.md`.
//! The file starts with a frontmatter block between two `---` lines, with
//! a `name` and a `description`, then the body in markdown:
//!
//! ```text
//! ---
//! name: triage-inbox
//! description: How Mark wants his inbox sorted. Load it before you triage.
//! ---
//! 1. Read the subject lines first. ...
//! ```
//!
//! The folder is the one source of the skills. The daemon puts them in the
//! profile overlay, so the engine lists each name and description in the
//! prompt and loads a body with `load_skill`. A skill that the engine
//! refuses (a bad name, a long description, a big body) shows with its
//! reason, and the others still load.
//!
//! The frontmatter reader takes the keys that a skill needs, `name`,
//! `description` and `author`, as plain, quoted or block (`>`, `|`) values.
//! It skips every other key, so a skill from another tool still loads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::SkillText;

/// The file of a skill in its folder.
pub const SKILL_FILE: &str = "SKILL.md";

/// The folder that holds retired skills, inside the skills folder.
pub const RETIRED: &str = ".retired";

/// One skill from a `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The name that the model loads it by.
    pub name: String,
    /// When to load it: one line in the prompt.
    pub description: String,
    /// The know-how.
    pub body: String,
    /// Who wrote it, when the file says.
    pub author: Option<String>,
    /// The file.
    pub path: PathBuf,
}

/// A `SKILL.md` that does not load, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// The file.
    pub path: PathBuf,
    /// Why it does not load.
    pub reason: String,
}

/// The skills of a folder: the ones that load, and the ones that do not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shelf {
    /// The skills that load, by name.
    pub skills: Vec<Skill>,
    /// The files that do not load.
    pub invalid: Vec<Invalid>,
}

impl Shelf {
    /// The skills as the engine takes them. Only the overlay sync of the
    /// refine loop needs them.
    #[must_use]
    pub fn texts(&self) -> BTreeMap<String, SkillText> {
        self.skills
            .iter()
            .map(|skill| {
                (
                    skill.name.clone(),
                    SkillText::new(skill.description.as_str(), skill.body.as_str()),
                )
            })
            .collect()
    }
}

/// The value of a frontmatter line: quotes go, a block marker starts a block.
enum Value {
    Plain(String),
    Block { folded: bool },
}

fn value(raw: &str) -> Value {
    let raw = raw.trim();
    match raw.chars().next() {
        Some('>') => Value::Block { folded: true },
        Some('|') => Value::Block { folded: false },
        Some(quote @ ('"' | '\'')) if raw.len() >= 2 && raw.ends_with(quote) => {
            let inner = &raw[1..raw.len() - 1];
            Value::Plain(if quote == '\'' {
                inner.replace("''", "'")
            } else {
                inner.replace("\\\"", "\"")
            })
        }
        _ => Value::Plain(raw.to_owned()),
    }
}

/// The skill in `text`, the content of the `SKILL.md` at `path`.
///
/// # Errors
///
/// Returns why the text is not a skill: no frontmatter, or no name or
/// description, or a name that the engine refuses.
pub fn parse(text: &str, path: &Path) -> Result<Skill, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(String::from(
            "the file does not start with a --- frontmatter line",
        ));
    }
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut block: Option<(String, bool, Vec<String>)> = None;
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        if let Some((key, folded, parts)) = block.as_mut() {
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                parts.push(line.trim().to_owned());
                continue;
            }
            let joined = if *folded {
                parts
                    .iter()
                    .filter(|part| !part.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                parts.join("\n").trim().to_owned()
            };
            keys.insert(std::mem::take(key), joined);
            block = None;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        if key.starts_with(' ') || key.starts_with('\t') {
            // A nested key of a key that a skill does not need.
            continue;
        }
        match value(raw) {
            Value::Plain(text) => {
                keys.insert(key.trim().to_owned(), text);
            }
            Value::Block { folded } => block = Some((key.trim().to_owned(), folded, Vec::new())),
        }
    }
    if let Some((key, folded, parts)) = block {
        let joined = if folded {
            parts
                .iter()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            parts.join("\n").trim().to_owned()
        };
        keys.insert(key, joined);
    }
    if !closed {
        return Err(String::from("the frontmatter has no closing --- line"));
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
    let name = keys
        .remove("name")
        .filter(|name| !name.is_empty())
        .ok_or("the frontmatter has no name")?;
    let description = keys
        .remove("description")
        .filter(|text| !text.is_empty())
        .ok_or("the frontmatter has no description")?;
    let skill = Skill {
        name,
        description,
        body,
        author: keys.remove("author").filter(|author| !author.is_empty()),
        path: path.to_path_buf(),
    };
    check(&skill)?;
    Ok(skill)
}

/// Why the engine would refuse `skill`, or `Ok`.
///
/// # Errors
///
/// Returns the reason of the engine.
pub fn check(skill: &Skill) -> Result<(), String> {
    autumn_harvest_agent::skill::check(
        &skill.name,
        &SkillText::new(skill.description.as_str(), skill.body.as_str()),
    )
    .map_err(|reason| format!("the engine refuses it: {reason}"))
}

/// The `SKILL.md` text of a skill.
#[must_use]
pub fn render(name: &str, description: &str, body: &str, author: Option<&str>) -> String {
    let quoted = |text: &str| format!("\"{}\"", text.replace('"', "\\\""));
    let mut text = format!("---\nname: {name}\ndescription: {}\n", quoted(description));
    if let Some(author) = author {
        // A write to a `String` cannot fail.
        let _ = writeln!(text, "author: {author}");
    }
    text.push_str("---\n\n");
    text.push_str(body.trim());
    text.push('\n');
    text
}

/// The skills of the folder `dir`, by name. A folder that does not exist
/// holds none. Two files with one name: the first by folder name loads, and
/// the second shows as invalid.
///
/// # Errors
///
/// Returns an error when the folder cannot be read.
pub fn load(dir: &Path) -> Result<Shelf> {
    let mut shelf = Shelf::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(shelf),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.starts_with('.'))
        })
        .collect();
    folders.sort();
    for folder in folders {
        let path = folder.join(SKILL_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse(&text, &path) {
            Ok(skill) if shelf.skills.iter().any(|other| other.name == skill.name) => {
                shelf.invalid.push(Invalid {
                    path,
                    reason: format!("another skill is named {}", skill.name),
                });
            }
            Ok(skill) => shelf.skills.push(skill),
            Err(reason) => shelf.invalid.push(Invalid { path, reason }),
        }
    }
    let limit = autumn_harvest_agent::skill::MAX_SKILLS;
    if shelf.skills.len() > limit {
        for skill in shelf.skills.split_off(limit) {
            shelf.invalid.push(Invalid {
                path: skill.path,
                reason: format!("the engine takes at most {limit} skills"),
            });
        }
    }
    Ok(shelf)
}

/// Writes a skill to `<dir>/<name>/SKILL.md`. It replaces a skill of that
/// name, and the replaced file goes to the retired folder first.
///
/// # Errors
///
/// Returns an error for a skill that the engine refuses, or when a file
/// cannot be written.
pub fn write(
    dir: &Path,
    name: &str,
    description: &str,
    body: &str,
    author: &str,
) -> Result<PathBuf> {
    let path = dir.join(name).join(SKILL_FILE);
    let skill = Skill {
        name: name.to_owned(),
        description: description.to_owned(),
        body: body.to_owned(),
        author: Some(author.to_owned()),
        path: path.clone(),
    };
    if let Err(reason) = check(&skill) {
        bail!("{reason}");
    }
    if path.exists() {
        retire(dir, name)?;
    }
    std::fs::create_dir_all(dir.join(name))
        .with_context(|| format!("creating {}", dir.join(name).display()))?;
    std::fs::write(&path, render(name, description, body, Some(author)))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Moves the folder of the skill `name` to the retired folder, with a time
/// in its name. Nothing is deleted. Returns whether the skill was there.
///
/// # Errors
///
/// Returns an error when the folder cannot be moved.
pub fn retire(dir: &Path, name: &str) -> Result<bool> {
    let shelf = load(dir)?;
    let Some(skill) = shelf.skills.iter().find(|skill| skill.name == name) else {
        return Ok(false);
    };
    let Some(folder) = skill.path.parent() else {
        return Ok(false);
    };
    let retired = dir.join(RETIRED);
    std::fs::create_dir_all(&retired).with_context(|| format!("creating {}", retired.display()))?;
    // Two retires in one millisecond get two names.
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%S%3f")
        .to_string();
    let base = format!("{name}-{stamp}");
    let target = std::iter::once(base.clone())
        .chain((2..=u32::MAX).map(|n| format!("{base}-{n}")))
        .map(|folder| retired.join(folder))
        .find(|target| !target.exists())
        .unwrap_or_else(|| retired.join(base));
    std::fs::rename(folder, &target)
        .with_context(|| format!("moving {} to {}", folder.display(), target.display()))?;
    Ok(true)
}

/// The small words that say nothing about what a skill does.
const SMALL_WORDS: &[&str] = &[
    "a", "an", "and", "for", "in", "my", "of", "on", "the", "to", "with",
];

/// The words of a skill name, without small words, each without a plural
/// `s`.
fn words(name: &str) -> std::collections::BTreeSet<&str> {
    name.split('-')
        .filter(|word| !word.is_empty() && !SMALL_WORDS.contains(word))
        .map(|word| {
            word.strip_suffix('s')
                .filter(|stem| stem.len() >= 3 && !stem.ends_with('s'))
                .unwrap_or(word)
        })
        .collect()
}

/// Whether the skill names `a` and `b` share at least half of their words,
/// so they likely name one skill: `read-a-page` and `read-a-web-page`.
#[must_use]
pub fn near(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    let shared = a.intersection(&b).count();
    let all = a.union(&b).count();
    shared > 0 && shared * 2 >= all
}

/// The change stamp of the folder `dir`: each `SKILL.md` with its time and
/// length. The daemon syncs the skills when it changes.
#[must_use]
pub fn stamp(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut stamp: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(SKILL_FILE))
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            Some((path, meta.modified().ok()?, meta.len()))
        })
        .collect();
    stamp.sort();
    stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> PathBuf {
        PathBuf::from("/s/x/SKILL.md")
    }

    #[test]
    fn names_with_the_same_words_are_near() {
        assert!(near("read-a-page", "read-a-web-page"));
        assert!(
            near("research-an-idea", "research-ideas"),
            "a plural is the same word"
        );
        assert!(near("triage-inbox", "inbox-triage"));
        assert!(!near("triage-inbox", "weekly-digest"));
        assert!(!near("tidy-memory", "tidy-notes"));
        assert!(!near("debug-myself", "debug-a-run"));
        assert!(
            !near("a-the", "an-of"),
            "a name of small words only is near nothing"
        );
    }

    #[test]
    fn a_claude_style_skill_reads_with_its_keys() {
        let text = "---\nname: triage-inbox\ndescription: \"How Mark sorts mail: load it first.\"\nlicense: MIT\nmetadata:\n  version: 2\n---\n\n# Triage\n\n1. Subjects first.\n";
        let skill = parse(text, &at()).unwrap();
        assert_eq!(skill.name, "triage-inbox");
        assert_eq!(skill.description, "How Mark sorts mail: load it first.");
        assert_eq!(skill.body, "# Triage\n\n1. Subjects first.");
        assert_eq!(skill.author, None);
    }

    #[test]
    fn a_folded_description_joins_its_lines() {
        let text = "---\nname: deploy\ndescription: >\n  Steps to ship\n  the site.\nauthor: littleguy\n---\nGo.";
        let skill = parse(text, &at()).unwrap();
        assert_eq!(skill.description, "Steps to ship the site.");
        assert_eq!(skill.author.as_deref(), Some("littleguy"));
    }

    #[test]
    fn a_rendered_skill_reads_back() {
        let text = render(
            "weekly-digest",
            "Write the \"Friday\" digest.",
            "Body\n",
            Some("littleguy"),
        );
        let skill = parse(&text, &at()).unwrap();
        assert_eq!(skill.name, "weekly-digest");
        assert_eq!(skill.description, "Write the \"Friday\" digest.");
        assert_eq!(skill.body, "Body");
        assert_eq!(skill.author.as_deref(), Some("littleguy"));
    }

    #[test]
    fn a_file_that_is_not_a_skill_says_why() {
        for (text, why) in [
            ("# no frontmatter", "frontmatter"),
            ("---\nname: a\n", "closing"),
            ("---\ndescription: d\n---\nb", "no name"),
            ("---\nname: a\n---\nb", "no description"),
            (
                "---\nname: bad name!\ndescription: d\n---\nb",
                "engine refuses",
            ),
            (
                "---\nname: a\ndescription: two\n  lines? no: this is a key\n---\nb",
                "",
            ),
        ] {
            match parse(text, &at()) {
                Err(reason) => assert!(reason.contains(why), "{text}: {reason}"),
                Ok(skill) => assert!(why.is_empty(), "{text} loaded as {}", skill.name),
            }
        }
    }

    #[test]
    fn a_folder_loads_its_skills_and_names_the_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "alpha", "First.", "A", "mark").unwrap();
        write(dir.path(), "beta", "Second.", "B", "littleguy").unwrap();
        std::fs::create_dir(dir.path().join("broken")).unwrap();
        std::fs::write(dir.path().join("broken").join(SKILL_FILE), "nope").unwrap();
        std::fs::create_dir(dir.path().join("copy")).unwrap();
        std::fs::write(
            dir.path().join("copy").join(SKILL_FILE),
            render("alpha", "Again.", "A2", None),
        )
        .unwrap();
        let shelf = load(dir.path()).unwrap();
        let names: Vec<&str> = shelf
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(shelf.invalid.len(), 2);
        assert_eq!(shelf.texts()["beta"], SkillText::new("Second.", "B"));
        assert_eq!(load(&dir.path().join("none")).unwrap(), Shelf::default());
    }

    #[test]
    fn a_rewrite_or_a_retire_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "alpha", "One.", "v1", "mark").unwrap();
        write(dir.path(), "alpha", "Two.", "v2", "littleguy").unwrap();
        let shelf = load(dir.path()).unwrap();
        assert_eq!(shelf.skills[0].body, "v2");
        assert!(retire(dir.path(), "alpha").unwrap());
        assert!(!retire(dir.path(), "alpha").unwrap());
        assert_eq!(load(dir.path()).unwrap().skills, []);
        let kept = std::fs::read_dir(dir.path().join(RETIRED)).unwrap().count();
        assert_eq!(kept, 2, "both old versions are kept");
    }

    #[test]
    fn retires_in_one_millisecond_keep_both_folders() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..5 {
            write(dir.path(), "alpha", "One.", &format!("v{n}"), "mark").unwrap();
            assert!(retire(dir.path(), "alpha").unwrap());
        }
        let kept = std::fs::read_dir(dir.path().join(RETIRED)).unwrap().count();
        assert_eq!(kept, 5);
    }

    #[test]
    fn a_change_moves_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let empty = stamp(dir.path());
        write(dir.path(), "alpha", "One.", "v1", "mark").unwrap();
        assert_ne!(stamp(dir.path()), empty);
    }
}
