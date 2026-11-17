use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::cli::KeysCommand;
use super::*;

/// A base64 key of 32 bytes of `byte`.
fn key(byte: u8) -> String {
    STANDARD.encode([byte; 32])
}

/// An environment of `pairs` only.
fn env(pairs: &[(&str, &str)]) -> Env {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Arc::new(move |name: &str| {
        pairs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.clone())
    })
}

/// A database path in a new folder.
fn db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lg.db");
    (dir, db)
}

/// Runs `command` with `input` on standard input.
fn run(command: KeysCommand, db: &Path, env: &Env, input: &str) -> Result<String> {
    command.run(&Store::beside(db), None, env, input.as_bytes())
}

fn set(db: &Path, env: &Env, name: &str, value: &str) {
    run(
        KeysCommand::Set {
            name: name.to_owned(),
        },
        db,
        env,
        value,
    )
    .unwrap();
}

#[test]
fn with_no_file_each_secret_comes_from_the_environment() {
    let (_dir, db) = db();
    let keys = HouseKeys::load(&db, env(&[("LG_BRAVE_API_KEY", "brave")])).unwrap();
    assert_eq!(keys.get("LG_BRAVE_API_KEY").as_deref(), Some("brave"));
    assert_eq!(keys.get("ANTHROPIC_API_KEY"), None);
    assert_eq!(keys.sealed_names(), [] as [&str; 0]);
    assert!(!path_for(&db).exists(), "a load writes no file");
}

#[test]
fn a_sealed_key_opens_and_wins_over_the_environment() {
    let (_dir, db) = db();
    let ring_env = env(&[(KEY_VAR, &key(7))]);
    set(&db, &ring_env, "ANTHROPIC_API_KEY", "sk-sealed\n");
    let text = std::fs::read_to_string(path_for(&db)).unwrap();
    assert!(!text.contains("sk-sealed"), "the file holds no plaintext");
    assert!(text.contains("sm1.k1."), "{text}");

    let both = env(&[
        (KEY_VAR, &key(7)),
        ("ANTHROPIC_API_KEY", "sk-old-env"),
        ("LG_BRAVE_API_KEY", "brave"),
    ]);
    let keys = HouseKeys::load(&db, both).unwrap();
    assert_eq!(keys.get("ANTHROPIC_API_KEY").as_deref(), Some("sk-sealed"));
    assert_eq!(keys.get("LG_BRAVE_API_KEY").as_deref(), Some("brave"));
    assert_eq!(keys.shadowed(), ["ANTHROPIC_API_KEY"]);
    let shown = format!("{keys:?}");
    assert!(shown.contains("ANTHROPIC_API_KEY"), "{shown}");
    assert!(!shown.contains("sk-sealed"), "{shown}");

    let lookup = Arc::new(keys).lookup();
    assert_eq!(lookup("ANTHROPIC_API_KEY").as_deref(), Some("sk-sealed"));
}

#[test]
fn the_file_is_private_to_the_owner() {
    let (_dir, db) = db();
    set(&db, &env(&[(KEY_VAR, &key(7))]), "LG_BRAVE_API_KEY", "b");
    let mode = std::fs::metadata(path_for(&db))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn sealed_keys_without_the_bootstrap_key_fail_the_start() {
    let (_dir, db) = db();
    set(&db, &env(&[(KEY_VAR, &key(7))]), "LG_BRAVE_API_KEY", "b");
    let error = HouseKeys::load(&db, env(&[("LG_BRAVE_API_KEY", "b")])).unwrap_err();
    assert!(format!("{error:#}").contains(KEY_VAR), "{error:#}");
}

#[test]
fn a_wrong_key_fails_the_start_and_names_the_key() {
    let (_dir, db) = db();
    set(&db, &env(&[(KEY_VAR, &key(7))]), "LG_BRAVE_API_KEY", "b");
    let error = HouseKeys::load(&db, env(&[(KEY_VAR, &key(8))])).unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("LG_BRAVE_API_KEY") && text.contains("`k1`"),
        "{text}"
    );
}

#[test]
fn a_value_moved_to_another_name_does_not_open() {
    let (_dir, db) = db();
    let ring_env = env(&[(KEY_VAR, &key(7))]);
    set(&db, &ring_env, "OPENAI_API_KEY", "sk-openai");
    let path = path_for(&db);
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let blob = doc["keys"]["OPENAI_API_KEY"].clone();
    doc["keys"]["LG_BRAVE_API_KEY"] = blob;
    std::fs::write(&path, doc.to_string()).unwrap();
    let error = HouseKeys::load(&db, ring_env).unwrap_err();
    assert!(
        format!("{error:#}").contains("LG_BRAVE_API_KEY"),
        "{error:#}"
    );
}

#[test]
fn rotation_opens_by_the_key_id_in_each_value() {
    let (_dir, db) = db();
    let old = env(&[(KEY_VAR, &key(1))]);
    set(&db, &old, "ANTHROPIC_API_KEY", "sk-a");
    set(&db, &old, "LG_BRAVE_API_KEY", "brave");

    // The new key without the retired one does not open the old values.
    let new_only = env(&[(KEY_VAR, &key(2)), (KEY_ID_VAR, "k2")]);
    assert!(HouseKeys::load(&db, Arc::clone(&new_only)).is_err());

    let retired = format!("k1={}", key(1));
    let rotating = env(&[
        (KEY_VAR, &key(2)),
        (KEY_ID_VAR, "k2"),
        (RETIRED_VAR, &retired),
    ]);
    // Both open while the rotation runs, before any reseal.
    let keys = HouseKeys::load(&db, Arc::clone(&rotating)).unwrap();
    assert_eq!(keys.get("ANTHROPIC_API_KEY").as_deref(), Some("sk-a"));
    // A new key goes under the active key.
    set(&db, &rotating, "OPENAI_API_KEY", "sk-o");
    let out = run(KeysCommand::Reseal, &db, &rotating, "").unwrap();
    assert!(
        out.contains("ANTHROPIC_API_KEY") && out.contains("LG_BRAVE_API_KEY"),
        "{out}"
    );
    assert!(
        !out.contains("OPENAI_API_KEY"),
        "a key under k2 stays: {out}"
    );
    let again = run(KeysCommand::Reseal, &db, &rotating, "").unwrap();
    assert!(again.contains("already"), "{again}");

    let list = run(KeysCommand::List, &db, &new_only, "").unwrap();
    assert_eq!(
        list,
        "ANTHROPIC_API_KEY\tk2\nLG_BRAVE_API_KEY\tk2\nOPENAI_API_KEY\tk2"
    );
    // The retired key can go now.
    let keys = HouseKeys::load(&db, new_only).unwrap();
    assert_eq!(keys.get("LG_BRAVE_API_KEY").as_deref(), Some("brave"));
    assert_eq!(keys.get("OPENAI_API_KEY").as_deref(), Some("sk-o"));
}

#[test]
fn a_failed_reseal_changes_no_key() {
    let (_dir, db) = db();
    set(&db, &env(&[(KEY_VAR, &key(1))]), "LG_BRAVE_API_KEY", "b");
    let before = std::fs::read_to_string(path_for(&db)).unwrap();
    let new_only = env(&[(KEY_VAR, &key(2)), (KEY_ID_VAR, "k2")]);
    assert!(run(KeysCommand::Reseal, &db, &new_only, "").is_err());
    assert_eq!(std::fs::read_to_string(path_for(&db)).unwrap(), before);
}

#[test]
fn the_ring_errors_never_show_a_key() {
    let short = STANDARD.encode([9_u8; 16]);
    for (pairs, needle) in [
        (vec![(KEY_VAR, short.as_str())], KEY_VAR),
        (vec![(KEY_VAR, "not base64 !")], KEY_VAR),
        (vec![(KEY_VAR, "QUFB"), (KEY_ID_VAR, "bad id")], KEY_VAR),
    ] {
        let error = ring(env(&pairs).as_ref()).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(needle), "{text}");
        assert!(!text.contains(&short), "{text}");
    }
    let good = key(3);
    let retired_bad = format!("k0={short}");
    let error = ring(env(&[(KEY_VAR, &good), (RETIRED_VAR, &retired_bad)]).as_ref()).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("`k0`") && !text.contains(&short), "{text}");
    let error = ring(env(&[(KEY_VAR, &good), (RETIRED_VAR, "k0")]).as_ref()).unwrap_err();
    assert!(format!("{error:#}").contains("id=base64"));
    let twice = format!("k1={good}");
    assert!(ring(env(&[(KEY_VAR, &good), (RETIRED_VAR, &twice)]).as_ref()).is_err());
    assert!(ring(env(&[]).as_ref()).unwrap().is_none());
    assert!(ring(env(&[(KEY_VAR, "  ")]).as_ref()).unwrap().is_none());
    let rotating = ring(
        env(&[
            (KEY_VAR, &good),
            (KEY_ID_VAR, "k2"),
            (
                RETIRED_VAR,
                " k1=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=, ",
            ),
        ])
        .as_ref(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(rotating.active_key_id(), "k2");
}

#[test]
fn set_reads_standard_input_and_takes_only_variable_names() {
    let (_dir, db) = db();
    let ring_env = env(&[(KEY_VAR, &key(7))]);
    set(&db, &ring_env, "LG_BRAVE_API_KEY", "  spaced value \r\n");
    let keys = HouseKeys::load(&db, Arc::clone(&ring_env)).unwrap();
    assert_eq!(
        keys.get("LG_BRAVE_API_KEY").as_deref(),
        Some("  spaced value ")
    );

    let set_of = |name: &str, input: &str| {
        run(
            KeysCommand::Set {
                name: name.to_owned(),
            },
            &db,
            &ring_env,
            input,
        )
    };
    assert!(format!("{:#}", set_of("LG_BRAVE_API_KEY", "\n").unwrap_err()).contains("empty"));
    for bad in [KEY_VAR, KEY_ID_VAR, RETIRED_VAR, "lower", "1X", ""] {
        assert!(set_of(bad, "v").is_err(), "{bad}");
    }
    let no_ring = run(
        KeysCommand::Set {
            name: String::from("X"),
        },
        &db,
        &env(&[]),
        "v",
    )
    .unwrap_err();
    assert!(format!("{no_ring:#}").contains(KEY_VAR));
}

#[test]
fn list_and_remove_show_no_value() {
    let (_dir, db) = db();
    let ring_env = env(&[(KEY_VAR, &key(7))]);
    let empty = run(KeysCommand::List, &db, &env(&[]), "").unwrap();
    assert!(empty.starts_with("no sealed keys"), "{empty}");
    set(&db, &ring_env, "DISCORD_BOT_TOKEN", "discord-secret");
    // Listing needs no ring.
    let list = run(KeysCommand::List, &db, &env(&[]), "").unwrap();
    assert_eq!(list, "DISCORD_BOT_TOKEN\tk1");
    let remove = |name: &str| {
        run(
            KeysCommand::Remove {
                name: name.to_owned(),
            },
            &db,
            &env(&[]),
            "",
        )
    };
    assert!(remove("DISCORD_BOT_TOKEN").unwrap().contains("removed"));
    assert!(remove("DISCORD_BOT_TOKEN").is_err());
    // An empty file needs no ring at start.
    let keys = HouseKeys::load(&db, env(&[])).unwrap();
    assert_eq!(keys.sealed_names(), [] as [&str; 0]);
}

#[test]
fn import_seals_the_known_keys_and_the_config_names() {
    let (_dir, db) = db();
    let config = littleguy_core::Config::parse(
        r#"
owner = "mark"

[hooks]
token_env = "LG_TEST_HOOK_TOKEN"
"#,
    )
    .unwrap();
    let shell = env(&[
        (KEY_VAR, &key(7)),
        ("ANTHROPIC_API_KEY", "sk-a"),
        ("LG_BRAVE_API_KEY", "brave"),
        ("LG_TEST_HOOK_TOKEN", "hook"),
        ("PATH", "/bin"),
    ]);
    let out = KeysCommand::Import { names: Vec::new() }
        .run(&Store::beside(&db), Some(&config), &shell, &b""[..])
        .unwrap();
    assert!(out.contains("Remove each one"), "{out}");
    let list = run(KeysCommand::List, &db, &shell, "").unwrap();
    assert_eq!(
        list,
        "ANTHROPIC_API_KEY\tk1\nLG_BRAVE_API_KEY\tk1\nLG_TEST_HOOK_TOKEN\tk1"
    );
    // The daemon reads them with an empty environment beside the ring.
    let keys = HouseKeys::load(&db, env(&[(KEY_VAR, &key(7))])).unwrap();
    assert_eq!(keys.get("LG_TEST_HOOK_TOKEN").as_deref(), Some("hook"));
    assert_eq!(keys.get("PATH"), None, "PATH is not a house key");
}

#[test]
fn a_named_import_of_an_unset_variable_seals_nothing() {
    let (_dir, db) = db();
    let shell = env(&[(KEY_VAR, &key(7)), ("OPENAI_API_KEY", "sk-o")]);
    let import = |names: &[&str]| {
        run(
            KeysCommand::Import {
                names: names.iter().map(|name| (*name).to_owned()).collect(),
            },
            &db,
            &shell,
            "",
        )
    };
    let error = import(&["OPENAI_API_KEY", "OPENROUTER_API_KEY"]).unwrap_err();
    assert!(format!("{error:#}").contains("OPENROUTER_API_KEY"));
    assert!(import(&["OPENAI_API_KEY", KEY_VAR]).is_err());
    assert!(!path_for(&db).exists(), "nothing was sealed");
    assert!(
        import(&["OPENAI_API_KEY"])
            .unwrap()
            .contains("OPENAI_API_KEY")
    );
    let nothing = run(
        KeysCommand::Import { names: Vec::new() },
        &db,
        &env(&[(KEY_VAR, &key(7))]),
        "",
    )
    .unwrap();
    assert!(nothing.contains("nothing was sealed"), "{nothing}");
}

#[test]
fn a_file_of_another_version_does_not_read() {
    let (_dir, db) = db();
    std::fs::write(path_for(&db), r#"{"version": 2, "keys": {}}"#).unwrap();
    let error = HouseKeys::load(&db, env(&[(KEY_VAR, &key(7))])).unwrap_err();
    assert!(format!("{error:#}").contains("version 2"), "{error:#}");
    std::fs::write(path_for(&db), "not json").unwrap();
    assert!(HouseKeys::load(&db, env(&[])).is_err());
}

#[test]
fn config_names_holds_each_secret_variable_of_the_config() {
    let config = littleguy_core::Config::parse(
        r#"
owner = "mark"

[hooks]
token_env = "LG_HOOK_TOKEN"

[[mcp.server]]
id = "notes"
url = "https://example.com/mcp"
bearer_env = "LG_NOTES_TOKEN"

[[mcp.server]]
id = "local"
command = "notes-mcp"
env = ["NOTES_KEY", "LG_NOTES_TOKEN"]
"#,
    )
    .unwrap();
    assert_eq!(
        config_names(&config),
        ["LG_HOOK_TOKEN", "LG_NOTES_TOKEN", "NOTES_KEY"]
    );
}
