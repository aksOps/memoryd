//! `memoryd integrate`: auto-discover installed AI coding agents and register
//! the memoryd MCP server into each one's config, plus a session-end `dream`
//! consolidation hook where the agent supports one (Claude Code: SessionEnd
//! shell hook; OpenCode: a JS plugin file firing on session.idle).
//!
//! Zero new dependencies and safe-by-construction:
//! - JSON agents (Claude Code, OpenCode) are deep-merged via `serde_json` —
//!   the existing config is parsed, the `memoryd` entry inserted/updated, and
//!   the file written back, so other servers and settings are preserved.
//! - TOML/YAML agents (Codex, Hermes) get an append-or-print: a minimal file
//!   is written when absent, registration is a no-op when already present, and
//!   otherwise the exact stanza is printed for a one-line manual paste rather
//!   than risk corrupting a config we can't safely deep-merge without a
//!   TOML/YAML parser (serde_yaml is unmaintained and would fail our gates).
//!
//! Every file that is modified is backed up to `<file>.memoryd.bak` first, and
//! `--dry-run` previews changes without writing anything.

use std::path::{Path, PathBuf};

/// Stable MCP server name registered into each agent.
const SERVER_NAME: &str = "memoryd";
/// Backup suffix appended before any in-place edit.
const BACKUP_SUFFIX: &str = ".memoryd.bak";

/// What `integrate` installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// MCP server registration + a session-end dream hook (default).
    Mcp,
    /// No MCP: full hook suite — capture (prompts + tool results), context
    /// injection (persona at session start, recall per prompt), and the
    /// session-end dream pass. For agents/users avoiding MCP entirely.
    Hooks,
    /// Everything: MCP tools and the full hook suite.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Per-user global config (default): the agent gets memoryd everywhere.
    User,
    /// Project config in the current directory.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntegrateArgs {
    /// `None` = every discovered agent; `Some(name)` = just that one.
    pub agent: Option<String>,
    pub scope: Scope,
    pub mode: Mode,
    pub dry_run: bool,
    /// Absolute memoryd binary path to register; resolved from current_exe if None.
    pub bin: Option<PathBuf>,
    /// Explicit `--db` to embed in the registered command; None = agent default.
    pub db: Option<PathBuf>,
}

/// The four agents this command knows how to integrate.
pub(crate) const KNOWN_AGENTS: [&str; 4] = ["claude", "opencode", "codex", "hermes"];

pub(crate) fn run(args: &IntegrateArgs) -> Result<(), crate::CliError> {
    let home = home_dir();
    let cwd = std::env::current_dir().map_err(crate::CliError::Io)?;
    let bin = match &args.bin {
        Some(bin) => bin.clone(),
        None => std::env::current_exe().map_err(crate::CliError::Io)?,
    };
    let report = plan(&home, &cwd, &bin, args)?;
    apply_and_report(&report, args.dry_run)
}

/// One agent's resolved integration plan.
struct AgentPlan {
    agent: &'static str,
    detected: bool,
    actions: Vec<Action>,
}

/// A single config mutation (or a manual instruction when auto-merge is unsafe).
enum Action {
    /// Deep-merge or append into `path`; `new_contents` is the full file to write.
    Write {
        path: PathBuf,
        new_contents: String,
        what: &'static str,
        already: bool,
    },
    /// Could not safely auto-merge; print the stanza for the user to paste.
    Manual {
        path: PathBuf,
        what: &'static str,
        stanza: String,
    },
}

fn plan(
    home: &Path,
    cwd: &Path,
    bin: &Path,
    args: &IntegrateArgs,
) -> Result<Vec<AgentPlan>, crate::CliError> {
    let targets: Vec<&'static str> = match &args.agent {
        Some(name) => {
            let name = name.as_str();
            match KNOWN_AGENTS.iter().find(|a| **a == name) {
                Some(found) => vec![*found],
                None => {
                    return Err(crate::CliError::UnknownAgent(name.to_string()));
                }
            }
        }
        None => KNOWN_AGENTS.to_vec(),
    };

    let mut plans = Vec::new();
    for agent in targets {
        let detected = detect(agent, home);
        // When a specific agent was named, integrate even if discovery missed
        // it (the user asked explicitly); for `all`, only touch what's present.
        let act = detected || args.agent.is_some();
        let actions = if act {
            match agent {
                "claude" => plan_claude(home, cwd, bin, args)?,
                "opencode" => plan_opencode(home, cwd, bin, args)?,
                "codex" => plan_codex(home, bin, args)?,
                "hermes" => plan_hermes(home, bin, args)?,
                _ => unreachable!("target came from KNOWN_AGENTS"),
            }
        } else {
            Vec::new()
        };
        plans.push(AgentPlan {
            agent,
            detected,
            actions,
        });
    }
    Ok(plans)
}

/// Best-effort presence probe: an agent counts as installed if its config dir
/// (or a well-known config file) exists under HOME.
pub(crate) fn detect(agent: &str, home: &Path) -> bool {
    let probes: &[&str] = match agent {
        "claude" => &[".claude", ".claude.json"],
        "opencode" => &[".config/opencode", ".opencode"],
        "codex" => &[".codex"],
        "hermes" => &[".hermes"],
        _ => &[],
    };
    probes.iter().any(|p| home.join(p).exists())
}

/// The command an agent should spawn to reach memoryd's MCP server.
fn mcp_command(bin: &Path, db: &Option<PathBuf>) -> Vec<String> {
    let mut cmd = vec![bin.display().to_string(), "mcp".to_string()];
    if let Some(db) = db {
        cmd.push("--db".to_string());
        cmd.push(db.display().to_string());
    }
    cmd
}

// ---- Claude Code: JSON MCP + SessionEnd hook ----------------------------

fn plan_claude(
    home: &Path,
    cwd: &Path,
    bin: &Path,
    args: &IntegrateArgs,
) -> Result<Vec<Action>, crate::CliError> {
    let cmd = mcp_command(bin, &args.db);
    let server = serde_json::json!({
        "type": "stdio",
        "command": cmd[0],
        "args": cmd[1..],
    });

    let (mcp_path, settings_path) = match args.scope {
        Scope::User => (
            home.join(".claude.json"),
            home.join(".claude/settings.json"),
        ),
        Scope::Project => (cwd.join(".mcp.json"), cwd.join(".claude/settings.json")),
    };

    let mut actions = Vec::new();
    if args.mode != Mode::Hooks {
        actions.push(merge_json_mcp(
            &mcp_path,
            "mcpServers",
            &server,
            "MCP server",
        )?);
    }

    // SessionEnd dream hook always; the full capture/inject suite in
    // hooks/all mode (Claude supports context injection on SessionStart and
    // UserPromptSubmit, and capture on PostToolUse).
    let mut entries = vec![("SessionEnd", dream_command(bin, &args.db))];
    if args.mode != Mode::Mcp {
        entries.push((
            "SessionStart",
            hook_command(bin, "session-start", "claude", &args.db),
        ));
        entries.push((
            "UserPromptSubmit",
            hook_command(bin, "prompt", "claude", &args.db),
        ));
        entries.push(("PostToolUse", hook_command(bin, "tool", "claude", &args.db)));
    }
    actions.push(merge_claude_hooks(&settings_path, &entries)?);
    Ok(actions)
}

/// `memoryd hook <verb> --agent <label>` command line for hook installs.
fn hook_command(bin: &Path, verb: &str, agent: &str, db: &Option<PathBuf>) -> String {
    let mut cmd = format!("{} hook {verb} --agent {agent}", bin.display());
    if let Some(db) = db {
        cmd.push_str(&format!(" --db {}", db.display()));
    }
    cmd
}

fn dream_command(bin: &Path, db: &Option<PathBuf>) -> String {
    match db {
        Some(db) => format!("{} dream --db {}", bin.display(), db.display()),
        None => format!("{} dream", bin.display()),
    }
}

/// Merge hook entries (`event` -> shell `command`) into a Claude settings
/// file. Idempotency is binary-path-independent: any existing memoryd hook
/// command for the event (recognized by its " dream" / " hook <verb>" tail
/// on a command that names `memoryd`) counts as present even if the binary
/// moved. An unrelated tool whose command merely ends in ` dream` does not.
fn merge_claude_hooks(path: &Path, entries: &[(&str, String)]) -> Result<Action, crate::CliError> {
    let mut root = read_json_object(path)?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or(crate::CliError::IntegrateConflict("hooks is not an object"))?;

    let mut all_already = true;
    for (event, command) in entries {
        let tail = memoryd_command_tail(command);
        let event_hooks = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| serde_json::json!([]));
        let arr = event_hooks
            .as_array_mut()
            .ok_or(crate::CliError::IntegrateConflict(
                "a hooks event entry is not an array",
            ))?;
        let already = arr.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                            c == command || (c.contains("memoryd") && c.ends_with(&tail))
                        })
                    })
                })
                .unwrap_or(false)
        });
        if !already {
            all_already = false;
            arr.push(serde_json::json!({
                "matcher": "",
                "hooks": [ { "type": "command", "command": command, "timeout": 60 } ],
            }));
        }
    }
    Ok(Action::Write {
        path: path.to_path_buf(),
        new_contents: to_pretty_json(&serde_json::Value::Object(root))?,
        what: "memoryd hooks",
        already: all_already,
    })
}

/// The binary-path-independent suffix of a memoryd hook command: everything
/// from the subcommand onward (" dream", " hook tool --agent claude", ...).
fn memoryd_command_tail(command: &str) -> String {
    for marker in [" dream", " hook "] {
        if let Some(index) = command.find(marker) {
            return command[index..].to_string();
        }
    }
    command.to_string()
}

// ---- OpenCode: JSON MCP (no JSON shell hook) ----------------------------

fn plan_opencode(
    home: &Path,
    cwd: &Path,
    bin: &Path,
    args: &IntegrateArgs,
) -> Result<Vec<Action>, crate::CliError> {
    let cmd = mcp_command(bin, &args.db);
    // OpenCode: `command` is an ARRAY (exe first), env key is `environment`,
    // and stdio servers are `type: "local"`.
    let server = serde_json::json!({
        "type": "local",
        "command": cmd,
        "enabled": true,
    });
    let path = match args.scope {
        Scope::User => home.join(".config/opencode/opencode.json"),
        Scope::Project => cwd.join("opencode.json"),
    };
    let mut actions = Vec::new();
    if args.mode != Mode::Hooks {
        actions.push(merge_json_mcp(&path, "mcp", &server, "MCP server")?);
    }

    // OpenCode lifecycle hooks are JS plugins auto-loaded from a plugins dir;
    // installing one is a standalone new file (nothing existing to corrupt).
    // It runs a dream pass on session.idle — incremental and frontier-based,
    // so repeat fires are cheap no-ops when nothing is pending.
    let plugin_path = match args.scope {
        Scope::User => home.join(".config/opencode/plugins/memoryd.js"),
        Scope::Project => cwd.join(".opencode/plugins/memoryd.js"),
    };
    actions.push(opencode_plugin(&plugin_path, bin, &args.db)?);
    Ok(actions)
}

/// Write the session-idle dream plugin if absent; never overwrite an existing
/// (possibly user-customized) plugin file.
fn opencode_plugin(
    path: &Path,
    bin: &Path,
    db: &Option<PathBuf>,
) -> Result<Action, crate::CliError> {
    if path.exists() {
        let existing = std::fs::read_to_string(path).map_err(crate::CliError::Io)?;
        return Ok(Action::Write {
            path: path.to_path_buf(),
            new_contents: existing,
            what: "session-idle dream plugin",
            already: true,
        });
    }
    let db_arg = match db {
        Some(db) => format!(" --db \"{}\"", shell_escape_db(&db.display().to_string())),
        None => String::new(),
    };
    let contents = format!(
        "// Installed by `memoryd integrate`. Consolidates this machine's memoryd\n\
         // captures (dream pass: distill, associate, decay) whenever a session goes\n\
         // idle. The pass is incremental and bounded, so repeat fires are cheap.\n\
         // Safe to edit or delete; re-running `memoryd integrate` will not overwrite.\n\
         export const MemorydPlugin = async ({{ $ }}) => {{\n\
         \x20 return {{\n\
         \x20   event: async ({{ event }}) => {{\n\
         \x20     if (event.type === \"session.idle\") {{\n\
         \x20       try {{\n\
         \x20         await $`\"{bin}\" dream{db_arg}`\n\
         \x20       }} catch (_) {{}}\n\
         \x20     }}\n\
         \x20   }},\n\
         \x20 }}\n\
         }}\n",
        bin = bin.display(),
    );
    Ok(Action::Write {
        path: path.to_path_buf(),
        new_contents: contents,
        what: "session-idle dream plugin",
        already: false,
    })
}

/// Escape a path for embedding inside a double-quoted string in the generated
/// JS plugin's shell template literal. Backslash, double-quote, dollar, and
/// backtick would otherwise terminate the quoted string early or trigger
/// shell/JS substitution, silently producing a broken plugin.
/// (The TOML/YAML stanzas interpolate the db path via `{:?}`, whose Debug
/// formatting already escapes `"` and `\`, so they need no extra treatment.)
fn shell_escape_db(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- Codex: TOML MCP + Stop hook (safe append) ---------------------------

fn plan_codex(
    home: &Path,
    bin: &Path,
    args: &IntegrateArgs,
) -> Result<Vec<Action>, crate::CliError> {
    let cmd = mcp_command(bin, &args.db);
    let arg_list = cmd[1..]
        .iter()
        .map(|a| format!("{:?}", a))
        .collect::<Vec<_>>()
        .join(", ");
    let mcp_stanza = format!(
        "[mcp_servers.{SERVER_NAME}]\ncommand = {:?}\nargs = [{arg_list}]\n",
        cmd[0]
    );
    // Codex's `Stop` event fires when a turn stops (its closest session-end
    // analog). `[[hooks.Stop]]` is a TOML array of tables, so appending a new
    // element is always valid even when other Stop hooks already exist.
    let dream = dream_command(bin, &args.db);
    let hook_stanza = format!(
        "[[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = {dream:?}\ntimeout = 60\n",
    );
    let path = home.join(".codex/config.toml");
    let mut parts = Vec::new();
    if args.mode != Mode::Hooks {
        parts.push(Part {
            present_marker: format!("[mcp_servers.{SERVER_NAME}]"),
            conflict_marker: None,
            stanza: mcp_stanza,
            what: "MCP server (TOML)",
        });
    }
    parts.push(Part {
        // Path-independent: one dream hook per file, even if the
        // binary moved since the last `integrate` run.
        present_marker: " dream\"".to_string(),
        conflict_marker: None,
        stanza: hook_stanza,
        what: "Stop dream hook (TOML)",
    });
    if args.mode != Mode::Mcp {
        // Codex mirrors Claude's hook model: capture + context injection.
        for (event, verb) in [
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt"),
            ("PostToolUse", "tool"),
        ] {
            let command = hook_command(bin, verb, "codex", &args.db);
            parts.push(Part {
                present_marker: format!(" hook {verb} "),
                conflict_marker: None,
                stanza: format!(
                    "[[hooks.{event}]]\n\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {command:?}\ntimeout = 60\n",
                ),
                what: "capture/context hooks (TOML)",
            });
        }
    }
    append_file_parts(&path, &parts)
}

// ---- Hermes: YAML MCP + on_session_end hook (safe append) ----------------

fn plan_hermes(
    home: &Path,
    bin: &Path,
    args: &IntegrateArgs,
) -> Result<Vec<Action>, crate::CliError> {
    let cmd = mcp_command(bin, &args.db);
    let arg_list = cmd[1..]
        .iter()
        .map(|a| format!("{:?}", a))
        .collect::<Vec<_>>()
        .join(", ");
    // Top-level `mcp_servers:` map; stdio is implicit (no type key).
    let mcp_stanza = format!(
        "mcp_servers:\n  {SERVER_NAME}:\n    command: {:?}\n    args: [{arg_list}]\n",
        cmd[0]
    );
    // Hermes has a true session-end lifecycle hook; in hooks/all mode the
    // post_tool_call capture rides in the SAME stanza (one top-level
    // `hooks:` key — two appends would be a duplicate-key YAML error).
    // Hermes documents no context-injection mechanism, so capture-only.
    let dream = dream_command(bin, &args.db);
    let mut hook_stanza =
        format!("hooks:\n  on_session_end:\n    - command: {dream:?}\n      timeout: 60\n");
    if args.mode != Mode::Mcp {
        let tool = hook_command(bin, "tool", "hermes", &args.db);
        hook_stanza.push_str(&format!(
            "  post_tool_call:\n    - command: {tool:?}\n      timeout: 60\n"
        ));
    }
    let path = home.join(".hermes/config.yaml");
    let mut parts = Vec::new();
    if args.mode != Mode::Hooks {
        parts.push(Part {
            present_marker: format!("  {SERVER_NAME}:"),
            conflict_marker: Some("mcp_servers:".to_string()),
            stanza: mcp_stanza,
            what: "MCP server (YAML)",
        });
    }
    parts.push(Part {
        // Path-independent (see plan_codex).
        present_marker: " dream\"".to_string(),
        conflict_marker: Some("hooks:".to_string()),
        stanza: hook_stanza,
        what: "session hooks (YAML)",
    });
    append_file_parts(&path, &parts)
}

// ---- JSON merge core -----------------------------------------------------

/// Insert/update `servers[SERVER_NAME] = server` under the top-level `key`
/// object of a JSON config, preserving everything else.
fn merge_json_mcp(
    path: &Path,
    key: &str,
    server: &serde_json::Value,
    what: &'static str,
) -> Result<Action, crate::CliError> {
    let mut root = read_json_object(path)?;
    let map = root
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let map_obj = map
        .as_object_mut()
        .ok_or(crate::CliError::IntegrateConflict(
            "MCP key is not an object",
        ))?;
    let already = map_obj.get(SERVER_NAME) == Some(server);
    map_obj.insert(SERVER_NAME.to_string(), server.clone());
    Ok(Action::Write {
        path: path.to_path_buf(),
        new_contents: to_pretty_json(&serde_json::Value::Object(root))?,
        what,
        already,
    })
}

/// Read a JSON object from `path`, or an empty object if the file is absent.
/// A present-but-unparseable file is a hard error (we never clobber it).
fn read_json_object(
    path: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::CliError> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let text = std::fs::read_to_string(path).map_err(crate::CliError::Io)?;
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| crate::CliError::IntegrateUnparseable(path.display().to_string()))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(crate::CliError::IntegrateUnparseable(
            path.display().to_string(),
        )),
    }
}

fn to_pretty_json(value: &serde_json::Value) -> Result<String, crate::CliError> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

// ---- TOML/YAML safe append engine ----------------------------------------

/// One appendable block for a text config file.
struct Part {
    /// Substring whose presence — on a line that also names `memoryd` — means
    /// this part is already installed (see [`marker_present`]).
    present_marker: String,
    /// Top-level key that, when already present (line-anchored) without our
    /// marker, makes appending unsafe (duplicate YAML key) -> Manual. `None`
    /// means appending is always structurally valid (TOML tables / array of
    /// tables).
    conflict_marker: Option<String>,
    stanza: String,
    what: &'static str,
}

/// Append every missing `part` to `path` in one pass over one in-memory copy
/// (so multiple parts never clobber each other), emitting a single Write for
/// the file plus a Manual action per part that cannot be appended safely.
fn append_file_parts(path: &Path, parts: &[Part]) -> Result<Vec<Action>, crate::CliError> {
    let mut text = if path.exists() {
        std::fs::read_to_string(path).map_err(crate::CliError::Io)?
    } else {
        String::new()
    };
    let mut actions = Vec::new();
    let mut appended = false;
    for part in parts {
        if marker_present(&text, &part.present_marker) {
            continue; // already installed
        }
        let conflicted = part
            .conflict_marker
            .as_deref()
            .is_some_and(|key| has_top_level_key(&text, key));
        if conflicted {
            actions.push(Action::Manual {
                path: path.to_path_buf(),
                what: part.what,
                stanza: part.stanza.clone(),
            });
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part.stanza);
        appended = true;
    }
    actions.insert(
        0,
        Action::Write {
            path: path.to_path_buf(),
            new_contents: text,
            what: "memoryd config blocks",
            already: !appended,
        },
    );
    Ok(actions)
}

/// A part counts as installed only when some line carries both the marker and
/// the `memoryd` binary name, so an unrelated user hook whose command happens
/// to end in e.g. ` dream"` does not suppress installation. Matching stays
/// binary-path-independent: a hook installed from `/old/path/memoryd` is
/// still detected after the binary moves.
fn marker_present(text: &str, marker: &str) -> bool {
    text.lines()
        .any(|line| line.contains(marker) && line.contains("memoryd"))
}

/// Line-anchored top-level key probe (no leading indentation), so a nested
/// `hooks:` in some YAML sub-map does not count as a conflict.
fn has_top_level_key(text: &str, key: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(key) && !line.starts_with(' ') && !line.starts_with('\t'))
}

// ---- Apply + report ------------------------------------------------------

fn apply_and_report(plans: &[AgentPlan], dry_run: bool) -> Result<(), crate::CliError> {
    let mut summary = Vec::new();
    for plan in plans {
        if !plan.detected && plan.actions.is_empty() {
            println!("{}: not detected, skipped", plan.agent);
            continue;
        }
        for action in &plan.actions {
            match action {
                Action::Write {
                    path,
                    new_contents,
                    what,
                    already,
                } => {
                    if *already {
                        println!(
                            "{}: {} already present ({})",
                            plan.agent,
                            what,
                            path.display()
                        );
                        continue;
                    }
                    if dry_run {
                        println!("{}: would write {} to {}", plan.agent, what, path.display());
                    } else {
                        write_with_backup(path, new_contents)?;
                        println!("{}: installed {} -> {}", plan.agent, what, path.display());
                        summary.push(plan.agent);
                    }
                }
                Action::Manual { path, what, stanza } => {
                    println!(
                        "{}: {} needs a manual one-line add to {} (existing config has \
                         other entries — not auto-edited):\n{}",
                        plan.agent,
                        what,
                        path.display(),
                        indent(stanza)
                    );
                }
            }
        }
    }
    Ok(())
}

/// Atomic-ish write: back up an existing file to `<path><BACKUP_SUFFIX>`, create
/// parent dirs, write to a temp file, then rename over the target. The backup
/// is only taken when none exists yet, so repeat `integrate` runs never
/// overwrite the true pre-memoryd original with an already-modified config.
fn write_with_backup(path: &Path, contents: &str) -> Result<(), crate::CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(crate::CliError::Io)?;
    }
    if path.exists() {
        let backup = PathBuf::from(format!("{}{BACKUP_SUFFIX}", path.display()));
        if !backup.exists() {
            std::fs::copy(path, &backup).map_err(crate::CliError::Io)?;
        }
    }
    let tmp = PathBuf::from(format!("{}.memoryd.tmp", path.display()));
    std::fs::write(&tmp, contents).map_err(crate::CliError::Io)?;
    std::fs::rename(&tmp, path).map_err(crate::CliError::Io)?;
    Ok(())
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(scope: Scope) -> IntegrateArgs {
        IntegrateArgs {
            agent: None,
            scope,
            mode: Mode::Mcp,
            dry_run: false,
            bin: Some(PathBuf::from("/usr/local/bin/memoryd")),
            db: None,
        }
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn tmp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "memoryd-integ-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_merge_preserves_existing_servers_and_is_idempotent() {
        let home = tmp_home("claude");
        let mcp = home.join(".claude.json");
        write(
            &mcp,
            r#"{"mcpServers":{"other":{"type":"stdio","command":"x"}},"foo":1}"#,
        );

        let bin = PathBuf::from("/usr/local/bin/memoryd");
        let a = args(Scope::User);
        let action = &plan_claude(&home, &home, &bin, &a).unwrap()[0];
        let Action::Write {
            new_contents,
            already,
            ..
        } = action
        else {
            panic!("expected a write action");
        };
        assert!(!already);
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        // Existing server and unrelated key preserved; memoryd added.
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["foo"], 1);
        assert_eq!(
            v["mcpServers"]["memoryd"]["command"],
            "/usr/local/bin/memoryd"
        );
        assert_eq!(v["mcpServers"]["memoryd"]["args"][0], "mcp");

        // Re-running against the merged file is a no-op.
        write(&mcp, new_contents);
        let action2 = &plan_claude(&home, &home, &bin, &a).unwrap()[0];
        let Action::Write { already, .. } = action2 else {
            panic!("expected write");
        };
        assert!(already, "second run is idempotent");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_session_end_hook_is_added_once() {
        let home = tmp_home("claude-hook");
        let bin = PathBuf::from("/usr/local/bin/memoryd");
        let a = args(Scope::User);
        let actions = plan_claude(&home, &home, &bin, &a).unwrap();
        let Action::Write { new_contents, .. } = &actions[1] else {
            panic!("expected hook write");
        };
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        let cmd = &v["hooks"]["SessionEnd"][0]["hooks"][0]["command"];
        assert_eq!(cmd, "/usr/local/bin/memoryd dream");

        // Idempotent: feeding the result back adds no second hook entry.
        let settings = home.join(".claude/settings.json");
        write(&settings, new_contents);
        let actions2 = plan_claude(&home, &home, &bin, &a).unwrap();
        let Action::Write {
            new_contents: c2,
            already,
            ..
        } = &actions2[1]
        else {
            panic!("expected hook write");
        };
        assert!(already);
        let v2: serde_json::Value = serde_json::from_str(c2).unwrap();
        assert_eq!(v2["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_uses_command_array_and_environment_shape() {
        let home = tmp_home("opencode");
        let bin = PathBuf::from("/opt/memoryd");
        let a = args(Scope::User);
        let action = &plan_opencode(&home, &home, &bin, &a).unwrap()[0];
        let Action::Write { new_contents, .. } = action else {
            panic!("expected write");
        };
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        assert_eq!(v["mcp"]["memoryd"]["type"], "local");
        assert_eq!(v["mcp"]["memoryd"]["command"][0], "/opt/memoryd");
        assert_eq!(v["mcp"]["memoryd"]["command"][1], "mcp");
        assert_eq!(v["mcp"]["memoryd"]["enabled"], true);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_plugin_installed_fresh_and_never_overwritten() {
        let home = tmp_home("opencode-plugin");
        let bin = PathBuf::from("/opt/memoryd");
        let a = args(Scope::User);
        let actions = plan_opencode(&home, &home, &bin, &a).unwrap();
        let Action::Write {
            path,
            new_contents,
            already,
            ..
        } = &actions[1]
        else {
            panic!("expected plugin write");
        };
        assert!(!already);
        assert!(path.ends_with(".config/opencode/plugins/memoryd.js"));
        assert!(new_contents.contains("export const MemorydPlugin"));
        assert!(new_contents.contains(r#"event.type === "session.idle""#));
        assert!(new_contents.contains(r#"await $`"/opt/memoryd" dream`"#));

        // An existing plugin file (possibly user-edited) is never overwritten.
        write(path, "// customized by user\n");
        let actions2 = plan_opencode(&home, &home, &bin, &a).unwrap();
        let Action::Write {
            new_contents: c2,
            already,
            ..
        } = &actions2[1]
        else {
            panic!("expected plugin action");
        };
        assert!(already);
        assert!(c2.contains("customized by user"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_appends_table_when_absent_and_noops_when_present() {
        let home = tmp_home("codex");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let a = args(Scope::User);
        // Absent file -> fresh write.
        let action = &plan_codex(&home, &bin, &a).unwrap()[0];
        let Action::Write {
            new_contents,
            already,
            ..
        } = action
        else {
            panic!("expected write");
        };
        assert!(!already);
        assert!(new_contents.contains("[mcp_servers.memoryd]"));
        assert!(new_contents.contains(r#"command = "/usr/bin/memoryd""#));
        assert!(new_contents.contains(r#"args = ["mcp"]"#));
        assert!(new_contents.contains("[[hooks.Stop]]"), "turn-stop hook");
        assert!(new_contents.contains(r#"command = "/usr/bin/memoryd dream""#));

        // Present -> idempotent no-op.
        let path = home.join(".codex/config.toml");
        write(&path, new_contents);
        let action2 = &plan_codex(&home, &bin, &a).unwrap()[0];
        let Action::Write { already, .. } = action2 else {
            panic!("expected write");
        };
        assert!(already);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_existing_other_server_is_appended_not_corrupted() {
        let home = tmp_home("codex-other");
        let path = home.join(".codex/config.toml");
        write(
            &path,
            "model = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        );
        let bin = PathBuf::from("/usr/bin/memoryd");
        let action = &plan_codex(&home, &bin, &args(Scope::User)).unwrap()[0];
        // `[mcp_servers.other]` contains the marker `[mcp_servers.` only as a
        // prefix; our exact marker `[mcp_servers.memoryd]` is absent, so we
        // append a new table (valid TOML) rather than print-manual.
        let Action::Write { new_contents, .. } = action else {
            panic!("expected append write");
        };
        assert!(new_contents.contains("[mcp_servers.other]"), "kept other");
        assert!(new_contents.contains("[mcp_servers.memoryd]"), "added ours");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn hermes_prints_manual_for_mcp_but_still_appends_hook() {
        let home = tmp_home("hermes");
        let path = home.join(".hermes/config.yaml");
        write(&path, "mcp_servers:\n  other:\n    command: \"x\"\n");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let actions = plan_hermes(&home, &bin, &args(Scope::User)).unwrap();
        // Top-level mcp_servers already present -> the MCP part can't be
        // appended safely (duplicate YAML key) and becomes a Manual stanza...
        let Some(Action::Manual { stanza, .. }) =
            actions.iter().find(|a| matches!(a, Action::Manual { .. }))
        else {
            panic!("expected manual instruction for the MCP part");
        };
        assert!(stanza.contains("memoryd:"));
        // ...while the absent `hooks:` key is appended in the same pass.
        let Action::Write {
            new_contents,
            already,
            ..
        } = &actions[0]
        else {
            panic!("expected write");
        };
        assert!(!already);
        assert!(
            new_contents.contains("mcp_servers:\n  other:"),
            "kept other"
        );
        assert!(new_contents.contains("hooks:\n  on_session_end:"));
        assert!(new_contents.contains("/usr/bin/memoryd dream"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn hermes_existing_hooks_key_makes_hook_manual() {
        let home = tmp_home("hermes-hooks");
        let path = home.join(".hermes/config.yaml");
        write(&path, "hooks:\n  pre_tool_call:\n    - command: \"y\"\n");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let actions = plan_hermes(&home, &bin, &args(Scope::User)).unwrap();
        // MCP appends (no mcp_servers key); the hook goes Manual.
        let Action::Write { new_contents, .. } = &actions[0] else {
            panic!("expected write");
        };
        assert!(new_contents.contains("mcp_servers:"));
        assert!(
            !new_contents.contains("on_session_end"),
            "hook must not be appended under a duplicate hooks key"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Manual { what, .. } if what.contains("hook"))),
            "hook part is manual"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn hermes_writes_fresh_file_when_absent() {
        let home = tmp_home("hermes-fresh");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let action = &plan_hermes(&home, &bin, &args(Scope::User)).unwrap()[0];
        let Action::Write { new_contents, .. } = action else {
            panic!("expected write");
        };
        assert!(new_contents.starts_with("mcp_servers:"));
        assert!(new_contents.contains("    command: \"/usr/bin/memoryd\""));
        assert!(new_contents.contains("hooks:\n  on_session_end:"));
        assert!(new_contents.contains("- command: \"/usr/bin/memoryd dream\""));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn hooks_mode_installs_full_suite_without_mcp() {
        let home = tmp_home("hooks-mode");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let mut a = args(Scope::User);
        a.mode = Mode::Hooks;

        // Claude: no MCP write; settings carry all four events.
        let actions = plan_claude(&home, &home, &bin, &a).unwrap();
        assert_eq!(actions.len(), 1, "MCP registration skipped in hooks mode");
        let Action::Write {
            new_contents, path, ..
        } = &actions[0]
        else {
            panic!("expected hooks write");
        };
        assert!(path.ends_with(".claude/settings.json"));
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        for event in [
            "SessionEnd",
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
        ] {
            assert!(
                v["hooks"][event].is_array(),
                "missing {event} in {new_contents}"
            );
        }
        assert_eq!(
            v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "/usr/bin/memoryd hook prompt --agent claude"
        );

        // Codex: hook tables for the suite, no [mcp_servers.memoryd].
        let codex = plan_codex(&home, &bin, &a).unwrap();
        let Action::Write { new_contents, .. } = &codex[0] else {
            panic!("expected codex write");
        };
        assert!(!new_contents.contains("[mcp_servers.memoryd]"));
        for event in ["Stop", "SessionStart", "UserPromptSubmit", "PostToolUse"] {
            assert!(
                new_contents.contains(&format!("[[hooks.{event}]]")),
                "missing {event}: {new_contents}"
            );
        }

        // Hermes: one combined hooks stanza (single top-level key), no MCP.
        let hermes = plan_hermes(&home, &bin, &a).unwrap();
        let Action::Write { new_contents, .. } = &hermes[0] else {
            panic!("expected hermes write");
        };
        assert!(!new_contents.contains("mcp_servers:"));
        assert_eq!(
            new_contents.matches("hooks:").count(),
            1,
            "exactly one top-level hooks key: {new_contents}"
        );
        assert!(new_contents.contains("on_session_end:"));
        assert!(new_contents.contains("post_tool_call:"));
        assert!(new_contents.contains("hook tool --agent hermes"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_hook_idempotency_is_path_independent_per_event() {
        let home = tmp_home("hooks-idem");
        let bin1 = PathBuf::from("/usr/bin/memoryd");
        let mut a = args(Scope::User);
        a.mode = Mode::Hooks;
        let actions = plan_claude(&home, &home, &bin1, &a).unwrap();
        let Action::Write {
            new_contents, path, ..
        } = &actions[0]
        else {
            panic!("write");
        };
        write(path, new_contents);

        // Second run with a moved binary adds nothing.
        let bin2 = PathBuf::from("/opt/elsewhere/memoryd");
        let actions2 = plan_claude(&home, &home, &bin2, &a).unwrap();
        let Action::Write {
            new_contents: c2,
            already,
            ..
        } = &actions2[0]
        else {
            panic!("write");
        };
        assert!(already, "moved binary still counts as installed");
        let v: serde_json::Value = serde_json::from_str(c2).unwrap();
        assert_eq!(v["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unparseable_json_is_an_error_not_a_clobber() {
        let home = tmp_home("badjson");
        let mcp = home.join(".claude.json");
        write(&mcp, "{ this is not json");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let result = plan_claude(&home, &home, &bin, &args(Scope::User));
        assert!(matches!(
            result,
            Err(crate::CliError::IntegrateUnparseable(_))
        ));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn db_flag_is_threaded_into_registered_command() {
        let home = tmp_home("dbflag");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let mut a = args(Scope::User);
        a.db = Some(PathBuf::from("/data/m.db"));
        let action = &plan_opencode(&home, &home, &bin, &a).unwrap()[0];
        let Action::Write { new_contents, .. } = action else {
            panic!("expected write");
        };
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        let cmd = v["mcp"]["memoryd"]["command"].as_array().unwrap();
        assert_eq!(cmd[1], "mcp");
        assert_eq!(cmd[2], "--db");
        assert_eq!(cmd[3], "/data/m.db");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn backup_keeps_original_across_repeat_writes() {
        let home = tmp_home("backup");
        let path = home.join("config.json");
        write(&path, "original");
        write_with_backup(&path, "first edit").unwrap();
        write_with_backup(&path, "second edit").unwrap();
        let backup = PathBuf::from(format!("{}{BACKUP_SUFFIX}", path.display()));
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "original",
            "backup must keep the pre-memoryd original, not the first edit"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second edit");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_plugin_escapes_db_path_for_js_and_shell() {
        let home = tmp_home("opencode-escape");
        let bin = PathBuf::from("/opt/memoryd");
        let mut a = args(Scope::User);
        a.db = Some(PathBuf::from(r#"/da"ta/$mem.db"#));
        let actions = plan_opencode(&home, &home, &bin, &a).unwrap();
        let Action::Write { new_contents, .. } = &actions[1] else {
            panic!("expected plugin write");
        };
        assert!(
            new_contents.contains(r#" --db "/da\"ta/\$mem.db""#),
            "quote and dollar must be escaped: {new_contents}"
        );
        assert!(
            !new_contents.contains(r#"--db "/da"ta"#),
            "a raw quote would terminate the embedded string early: {new_contents}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn foreign_dream_hook_does_not_suppress_install() {
        let home = tmp_home("foreign-hook");
        let bin = PathBuf::from("/usr/bin/memoryd");
        let a = args(Scope::User);

        // Claude: an unrelated SessionEnd hook ending in " dream" is not ours.
        let settings = home.join(".claude/settings.json");
        write(
            &settings,
            r#"{"hooks":{"SessionEnd":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/other-tool dream"}]}]}}"#,
        );
        let actions = plan_claude(&home, &home, &bin, &a).unwrap();
        let Action::Write {
            new_contents,
            already,
            ..
        } = &actions[1]
        else {
            panic!("expected hook write");
        };
        assert!(!already, "foreign dream hook must not count as installed");
        let v: serde_json::Value = serde_json::from_str(new_contents).unwrap();
        assert_eq!(v["hooks"]["SessionEnd"].as_array().unwrap().len(), 2);

        // Codex: a non-memoryd Stop hook ending in ` dream"` is not ours either.
        let codex = home.join(".codex/config.toml");
        write(
            &codex,
            "[[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"/usr/bin/other-tool dream\"\ntimeout = 60\n",
        );
        let actions = plan_codex(&home, &bin, &a).unwrap();
        let Action::Write {
            new_contents,
            already,
            ..
        } = &actions[0]
        else {
            panic!("expected write");
        };
        assert!(!already);
        assert!(
            new_contents.contains(r#"command = "/usr/bin/memoryd dream""#),
            "memoryd dream hook must still be appended: {new_contents}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn moved_memoryd_dream_hook_is_still_detected() {
        let home = tmp_home("moved-hook");
        let codex = home.join(".codex/config.toml");
        write(
            &codex,
            "[mcp_servers.memoryd]\ncommand = \"/some/old/path/memoryd\"\nargs = [\"mcp\"]\n\n\
             [[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"/some/old/path/memoryd dream\"\ntimeout = 60\n",
        );
        let bin = PathBuf::from("/new/path/memoryd");
        let actions = plan_codex(&home, &bin, &args(Scope::User)).unwrap();
        let Action::Write { already, .. } = &actions[0] else {
            panic!("expected write");
        };
        assert!(already, "old-path memoryd hook still counts as installed");
        let _ = std::fs::remove_dir_all(&home);
    }
}
