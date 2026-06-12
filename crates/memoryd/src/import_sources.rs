//! `memoryd import --source claude|codex|opencode|hermes`: discover each
//! agent's on-disk session history under `$HOME` and stage it through the
//! source-specific parsers in [`memoryd_core::importers`].
//!
//! Discovery is pure filesystem walking — every function takes `home` (and,
//! for OpenCode, an already-resolved `XDG_DATA_HOME`) as a parameter so tests
//! never touch the real environment, following the `integrate` module's
//! idioms. Orchestration in [`run_agent_import`] is lenient per file
//! (oversized or empty files become per-file notes, never a failed run) but
//! strict on database errors, and stops at the first *paused* batch so a full
//! embed queue pauses the run instead of flooding the worker; re-running
//! resumes where it left off thanks to content-hash dedup.

use memoryd_core::import::{ImportSummary, ImportUnit};
use memoryd_core::importers::{
    parse_claude_session, parse_codex_rollout, read_hermes_db, read_opencode_db,
};
use memoryd_core::store::{MAX_IMPORT_FILE_BYTES, Store, StoreError};
use std::path::{Path, PathBuf};

use crate::CliError;

/// Upper bound on session files collected per discovery walk; beyond this the
/// walk stops rather than ballooning memory on a pathological tree. Re-running
/// after the first files complete makes progress because already-staged
/// content dedups to `skipped`.
const MAX_IMPORT_FILES: usize = 4_096;

/// Recursion bound for the Codex sessions walk (`sessions/YYYY/MM/DD/*.jsonl`
/// needs 3; one extra level of slack, never unbounded).
const MAX_WALK_DEPTH: usize = 4;

/// One discovered file's import result: either a batch summary or a note
/// explaining why the file was skipped (or not found).
#[derive(Debug)]
pub(crate) struct ImportFileOutcome {
    pub path: String,
    pub summary: Option<ImportSummary>,
    pub note: Option<String>,
}

/// Everything one agent import produced, in file order.
#[derive(Debug)]
pub(crate) struct ImportRun {
    pub source: String,
    pub files: Vec<ImportFileOutcome>,
}

/// Claude Code session transcripts: `<home>/.claude/projects/*/*.jsonl`
/// (exactly one level of per-project directories), sorted for deterministic
/// staging order.
pub(crate) fn claude_session_files(home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(projects) = std::fs::read_dir(home.join(".claude/projects")) else {
        return files;
    };
    for project in projects.flatten() {
        if project.path().is_dir() {
            push_jsonl_in_dir(&project.path(), &mut files);
        }
    }
    files.sort();
    files
}

/// Codex CLI rollouts: a bounded recursive walk of `<home>/.codex/sessions`
/// (layout `YYYY/MM/DD/rollout-*.jsonl`) collecting `*.jsonl`, sorted.
pub(crate) fn codex_session_files(home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_recursive(&home.join(".codex/sessions"), MAX_WALK_DEPTH, &mut files);
    files.sort();
    files
}

/// The conventional OpenCode database path: `$XDG_DATA_HOME/opencode/opencode.db`
/// when the caller resolved an `XDG_DATA_HOME`, else
/// `<home>/.local/share/opencode/opencode.db`.
fn opencode_db_path(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    match xdg_data_home {
        Some(xdg) => xdg.join("opencode").join("opencode.db"),
        None => home.join(".local/share/opencode/opencode.db"),
    }
}

/// The OpenCode database, if present at its conventional path.
pub(crate) fn opencode_db(home: &Path, xdg_data_home: Option<&Path>) -> Option<PathBuf> {
    let path = opencode_db_path(home, xdg_data_home);
    path.is_file().then_some(path)
}

/// The conventional Hermes state database path.
fn hermes_db_path(home: &Path) -> PathBuf {
    home.join(".hermes/state.db")
}

/// The Hermes database, if present at its conventional path.
pub(crate) fn hermes_db(home: &Path) -> Option<PathBuf> {
    let path = hermes_db_path(home);
    path.is_file().then_some(path)
}

/// Collect `*.jsonl` files directly inside `dir` (no recursion), capped.
fn push_jsonl_in_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_IMPORT_FILES {
            return;
        }
        let path = entry.path();
        if is_jsonl_file(&path) {
            files.push(path);
        }
    }
}

/// Depth-bounded recursive `*.jsonl` collection; `depth` is the number of
/// directory levels left to descend below `dir`.
fn collect_jsonl_recursive(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_IMPORT_FILES {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_jsonl_recursive(&path, depth - 1, files);
            }
        } else if is_jsonl_file(&path) {
            files.push(path);
        }
    }
}

fn is_jsonl_file(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl")
}

/// Import one agent's history into `store`.
///
/// `override_path` replaces discovery: a file imports just that file; a
/// directory is searched with the agent's own matcher (Claude: direct and
/// one-level-deep `*.jsonl`; Codex: bounded recursive `*.jsonl`; OpenCode and
/// Hermes: the directory's conventional database filename). Without an
/// override, discovery runs against `home` (and `xdg_data_home` for
/// OpenCode). Unknown agent names reuse [`CliError::UnknownAgent`].
pub(crate) fn run_agent_import(
    store: &mut Store,
    agent: &str,
    override_path: Option<&Path>,
    home: &Path,
    xdg_data_home: Option<&Path>,
    max_active_jobs: usize,
) -> Result<ImportRun, CliError> {
    match agent {
        "claude" => {
            let files = match override_path {
                Some(path) if path.is_file() => vec![path.to_path_buf()],
                Some(path) => {
                    // A project tree override: accept both `<dir>/*.jsonl` and
                    // the on-disk `<dir>/<project>/*.jsonl` layout.
                    let mut files = Vec::new();
                    push_jsonl_in_dir(path, &mut files);
                    let Ok(entries) = std::fs::read_dir(path) else {
                        return Ok(empty_run("claude-session"));
                    };
                    for entry in entries.flatten() {
                        if entry.path().is_dir() {
                            push_jsonl_in_dir(&entry.path(), &mut files);
                        }
                    }
                    files.sort();
                    files
                }
                None => claude_session_files(home),
            };
            import_jsonl_files(
                store,
                "claude-session",
                &files,
                parse_claude_session,
                max_active_jobs,
            )
        }
        "codex" => {
            let files = match override_path {
                Some(path) if path.is_file() => vec![path.to_path_buf()],
                Some(path) => {
                    let mut files = Vec::new();
                    collect_jsonl_recursive(path, MAX_WALK_DEPTH, &mut files);
                    files.sort();
                    files
                }
                None => codex_session_files(home),
            };
            import_jsonl_files(
                store,
                "codex-session",
                &files,
                parse_codex_rollout,
                max_active_jobs,
            )
        }
        "opencode" => import_db(
            store,
            "opencode-session",
            resolve_db(override_path, "opencode.db", || {
                opencode_db(home, xdg_data_home)
                    .ok_or_else(|| opencode_db_path(home, xdg_data_home))
            }),
            read_opencode_db,
            max_active_jobs,
        ),
        "hermes" => import_db(
            store,
            "hermes-session",
            resolve_db(override_path, "state.db", || {
                hermes_db(home).ok_or_else(|| hermes_db_path(home))
            }),
            read_hermes_db,
            max_active_jobs,
        ),
        other => Err(CliError::UnknownAgent(other.to_string())),
    }
}

fn empty_run(source: &str) -> ImportRun {
    ImportRun {
        source: source.to_string(),
        files: Vec::new(),
    }
}

/// Resolve a SQLite agent's database: an override file is used as-is, an
/// override directory is probed for the conventional `db_name`, and no
/// override defers to `discover` (the agent's home-relative discovery).
/// `Err` carries the probed path for the "not found" note.
fn resolve_db(
    override_path: Option<&Path>,
    db_name: &str,
    discover: impl FnOnce() -> Result<PathBuf, PathBuf>,
) -> Result<PathBuf, PathBuf> {
    let candidate = match override_path {
        Some(path) if path.is_file() => return Ok(path.to_path_buf()),
        Some(path) => path.join(db_name),
        None => return discover(),
    };
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(candidate)
    }
}

/// Stage every discovered JSONL transcript through `parse`, one batch per
/// file. Oversized or content-free files become per-file notes; the loop
/// stops after the first paused batch (embed queue full) so a re-run resumes.
fn import_jsonl_files(
    store: &mut Store,
    source: &str,
    files: &[PathBuf],
    parse: fn(&str, &str) -> Vec<ImportUnit>,
    max_active_jobs: usize,
) -> Result<ImportRun, CliError> {
    let mut run = empty_run(source);
    for file in files {
        let path = file.display().to_string();
        let file_bytes = std::fs::metadata(file).map(|meta| meta.len()).unwrap_or(0);
        if file_bytes > MAX_IMPORT_FILE_BYTES {
            run.files.push(ImportFileOutcome {
                path,
                summary: None,
                note: Some("skipped: file exceeds 64 MiB cap".to_string()),
            });
            continue;
        }
        // Lenient per file, like the parsers themselves: a vanished or
        // non-UTF-8 transcript is noted and skipped, never a failed run.
        let contents = match std::fs::read_to_string(file) {
            Ok(contents) => contents,
            Err(err) => {
                run.files.push(ImportFileOutcome {
                    path,
                    summary: None,
                    note: Some(format!("skipped: unreadable ({err})")),
                });
                continue;
            }
        };
        let fallback_session = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "import".to_string());
        let units = parse(&contents, &fallback_session);
        if units.is_empty() {
            run.files.push(ImportFileOutcome {
                path,
                summary: None,
                note: Some("skipped: no importable content".to_string()),
            });
            continue;
        }
        let summary = store.import_units(source, &path, &units, max_active_jobs)?;
        let paused = summary.state == "paused";
        run.files.push(ImportFileOutcome {
            path,
            summary: Some(summary),
            note: None,
        });
        if paused {
            break; // embed queue full; the next run resumes from here
        }
    }
    Ok(run)
}

/// Stage one agent database (resolved by [`resolve_db`]) as a single batch.
/// A missing database is a per-run note; a read error is a hard error (the
/// database exists but cannot be imported, which the user must act on).
fn import_db(
    store: &mut Store,
    source: &str,
    resolved: Result<PathBuf, PathBuf>,
    read: fn(&Path) -> Result<Vec<ImportUnit>, memoryd_core::import::ImportError>,
    max_active_jobs: usize,
) -> Result<ImportRun, CliError> {
    let mut run = empty_run(source);
    let db = match resolved {
        Ok(db) => db,
        Err(probed) => {
            run.files.push(ImportFileOutcome {
                path: probed.display().to_string(),
                summary: None,
                note: Some(format!("not found: {}", probed.display())),
            });
            return Ok(run);
        }
    };
    let path = db.display().to_string();
    let units = read(&db).map_err(|err| CliError::Store(StoreError::from(err)))?;
    if units.is_empty() {
        run.files.push(ImportFileOutcome {
            path,
            summary: None,
            note: Some("skipped: no importable content".to_string()),
        });
        return Ok(run);
    }
    let summary = store.import_units(source, &path, &units, max_active_jobs)?;
    run.files.push(ImportFileOutcome {
        path,
        summary: Some(summary),
        note: None,
    });
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "memoryd-imports-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A two-record Claude session transcript whose user line carries `word`.
    fn claude_session_body(word: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s-{word}\",\"timestamp\":\"2024-01-15T12:30:45Z\",\"message\":{{\"content\":\"prefer {word} for migrations\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"s-{word}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"noted: {word}\"}}]}}}}\n"
        )
    }

    /// Open a store on a db file inside `home` so cleanup is one remove_dir_all.
    fn test_store(home: &Path) -> Store {
        Store::open(home.join("memoryd-test.db")).expect("store opens")
    }

    #[test]
    fn discovers_claude_session_files_sorted() {
        let home = tmp_home("claude-discover");
        write(&home.join(".claude/projects/proj-b/zz.jsonl"), "{}");
        write(&home.join(".claude/projects/proj-a/aa.jsonl"), "{}");
        write(&home.join(".claude/projects/proj-a/notes.txt"), "x");
        // Files directly under projects/ or nested two levels deep are out of
        // contract (`projects/*/*.jsonl` only).
        write(&home.join(".claude/projects/stray.jsonl"), "{}");
        write(&home.join(".claude/projects/proj-a/deep/d.jsonl"), "{}");

        let files = claude_session_files(&home);
        assert_eq!(
            files,
            vec![
                home.join(".claude/projects/proj-a/aa.jsonl"),
                home.join(".claude/projects/proj-b/zz.jsonl"),
            ]
        );
        // A home with no Claude tree discovers nothing instead of erroring.
        assert!(claude_session_files(&home.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn discovers_codex_rollouts_recursively_bounded() {
        let home = tmp_home("codex-discover");
        write(
            &home.join(".codex/sessions/2024/01/02/rollout-b.jsonl"),
            "{}",
        );
        write(
            &home.join(".codex/sessions/2024/01/01/rollout-a.jsonl"),
            "{}",
        );
        write(&home.join(".codex/sessions/top.jsonl"), "{}");
        write(&home.join(".codex/sessions/2024/01/02/skip.txt"), "x");
        // Five directory levels below sessions/ is past the depth bound.
        write(&home.join(".codex/sessions/a/b/c/d/e/too-deep.jsonl"), "{}");

        let files = codex_session_files(&home);
        assert_eq!(
            files,
            vec![
                home.join(".codex/sessions/2024/01/01/rollout-a.jsonl"),
                home.join(".codex/sessions/2024/01/02/rollout-b.jsonl"),
                home.join(".codex/sessions/top.jsonl"),
            ]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_db_prefers_xdg_data_home() {
        let home = tmp_home("opencode-discover");
        let xdg = home.join("xdg-data");
        write(&xdg.join("opencode/opencode.db"), "stub");
        write(&home.join(".local/share/opencode/opencode.db"), "stub");

        assert_eq!(
            opencode_db(&home, Some(&xdg)),
            Some(xdg.join("opencode/opencode.db"))
        );
        assert_eq!(
            opencode_db(&home, None),
            Some(home.join(".local/share/opencode/opencode.db"))
        );
        // XDG set but empty there: no silent fallback to ~/.local/share —
        // the conventional path under XDG simply does not exist.
        assert_eq!(opencode_db(&home, Some(&home.join("empty-xdg"))), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn hermes_db_detection_and_absence() {
        let home = tmp_home("hermes-discover");
        assert_eq!(hermes_db(&home), None);

        // Absent database: the run carries a single "not found" note pointing
        // at the conventional path, and no batch is created.
        let mut store = test_store(&home);
        let run = run_agent_import(&mut store, "hermes", None, &home, None, 100)
            .expect("absent db is a note, not an error");
        assert_eq!(run.source, "hermes-session");
        assert_eq!(run.files.len(), 1);
        assert!(run.files[0].summary.is_none());
        let note = run.files[0].note.as_deref().expect("note present");
        assert!(
            note.starts_with("not found: ") && note.contains(".hermes/state.db"),
            "note: {note}"
        );

        write(&home.join(".hermes/state.db"), "stub");
        assert_eq!(hermes_db(&home), Some(home.join(".hermes/state.db")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_agent_import_stages_claude_tree_end_to_end() {
        let home = tmp_home("claude-e2e");
        write(
            &home.join(".claude/projects/proj-a/s1.jsonl"),
            &claude_session_body("flyway"),
        );
        write(
            &home.join(".claude/projects/proj-b/s2.jsonl"),
            &claude_session_body("sqitch"),
        );
        let mut store = test_store(&home);

        let run = run_agent_import(&mut store, "claude", None, &home, None, 100)
            .expect("import succeeds");
        assert_eq!(run.source, "claude-session");
        assert_eq!(run.files.len(), 2);
        for outcome in &run.files {
            let summary = outcome.summary.as_ref().expect("summary per file");
            assert_eq!(summary.total, 2, "user + assistant unit per session");
            assert_eq!(summary.processed, 2);
            assert_eq!(summary.skipped, 0);
            assert_eq!(summary.state, "completed");
        }

        // Idempotent re-run: content-hash dedup skips everything.
        let rerun = run_agent_import(&mut store, "claude", None, &home, None, 100)
            .expect("re-import succeeds");
        for outcome in &rerun.files {
            let summary = outcome.summary.as_ref().expect("summary per file");
            assert_eq!(summary.processed, 0);
            assert_eq!(summary.skipped, summary.total);
            assert_eq!(summary.state, "completed");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_agent_import_skips_oversized_file_with_note() {
        let home = tmp_home("claude-oversize");
        write(
            &home.join(".claude/projects/proj/ok.jsonl"),
            &claude_session_body("vacuum"),
        );
        let big = home.join(".claude/projects/proj/zz-huge.jsonl");
        write(&big, "");
        // Sparse file: metadata reports the size without allocating 64 MiB.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(MAX_IMPORT_FILE_BYTES + 1)
            .unwrap();
        let mut store = test_store(&home);

        let run = run_agent_import(&mut store, "claude", None, &home, None, 100)
            .expect("import succeeds");
        assert_eq!(run.files.len(), 2);
        let ok = &run.files[0];
        assert!(ok.summary.is_some(), "small file imports normally");
        let skipped = &run.files[1];
        assert!(skipped.summary.is_none());
        assert_eq!(
            skipped.note.as_deref(),
            Some("skipped: file exceeds 64 MiB cap")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_agent_import_path_override_file_and_dir() {
        let home = tmp_home("claude-override");
        let tree = home.join("exported");
        write(&tree.join("direct.jsonl"), &claude_session_body("redis"));
        write(
            &tree.join("proj/nested.jsonl"),
            &claude_session_body("kafka"),
        );
        let mut store = test_store(&home);

        // File override: exactly that file, no discovery.
        let single = run_agent_import(
            &mut store,
            "claude",
            Some(&tree.join("direct.jsonl")),
            &home,
            None,
            100,
        )
        .expect("file override imports");
        assert_eq!(single.files.len(), 1);
        assert_eq!(single.files[0].summary.as_ref().expect("summary").total, 2);

        // Directory override: direct `*.jsonl` and one project level deep.
        let tree_run = run_agent_import(&mut store, "claude", Some(&tree), &home, None, 100)
            .expect("dir override imports");
        let mut paths: Vec<&str> = tree_run.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(tree_run.files.len(), 2);
        assert!(paths[0].ends_with("direct.jsonl"), "paths: {paths:?}");
        assert!(paths[1].ends_with("nested.jsonl"), "paths: {paths:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn run_agent_import_unknown_agent_errors() {
        let home = tmp_home("unknown-agent");
        let mut store = test_store(&home);
        let err = run_agent_import(&mut store, "emacs", None, &home, None, 100)
            .expect_err("unknown agent fails");
        assert!(matches!(err, CliError::UnknownAgent(name) if name == "emacs"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
