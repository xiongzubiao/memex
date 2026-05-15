//! `memex install` — register memex hooks and skills with detected AI agent CLIs.
//!
//! Hook entries are idempotent: any existing entry whose command contains one
//! of `MEMEX_SENTINELS` is removed before the fresh template is appended.
//! Hook and skill bodies are baked into the binary via `include_str!`; rebuild
//! after editing the source files under `plugin/`.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const CLAUDE_CODE_HOOKS: &str = include_str!("../../plugin/hooks/claude-code.json");
const CODEX_HOOKS: &str = include_str!("../../plugin/hooks/codex.json");
const GEMINI_CLI_HOOKS: &str = include_str!("../../plugin/hooks/gemini-cli.json");

const SKILLS: &[(&str, &str)] = &[
    (
        "memex-query",
        include_str!("../../plugin/skills/memex-query/SKILL.md"),
    ),
    (
        "memex-ingest",
        include_str!("../../plugin/skills/memex-ingest/SKILL.md"),
    ),
    (
        "memex-brainstorm",
        include_str!("../../plugin/skills/memex-brainstorm/SKILL.md"),
    ),
];

// OpenClaw pack layout: <pack>/package.json + <pack>/memex/{HOOK.md,handler.js}.
// The nested `memex/` subdir is required — OpenClaw's loader iterates the
// SUBDIRECTORIES of an extraDir entry, not the entry itself.
const OPENCLAW_PACKAGE_JSON: &str = include_str!("../../plugin/.openclaw-plugin/package.json");
const OPENCLAW_HOOK_MD: &str = include_str!("../../plugin/.openclaw-plugin/memex/HOOK.md");
const OPENCLAW_HANDLER_JS: &str = include_str!("../../plugin/.openclaw-plugin/memex/handler.js");

const HERMES_HOOK_YAML: &str = include_str!("../../plugin/.hermes-plugin/memex/HOOK.yaml");
const HERMES_HANDLER_PY: &str = include_str!("../../plugin/.hermes-plugin/memex/handler.py");

const OPENCODE_PLUGIN_TS: &str = include_str!("../../plugin/.opencode-plugin/memex.ts");

const MEMEX_SENTINELS: &[&str] = &[
    "memex daemon",
    "memex hook",
    "memex ingest",
    "memex-wrapper",
];

/// Single source of truth for which directory under `~` indicates each agent.
/// OpenCode lives under `~/.config/opencode/` (XDG-style), not `~/.opencode/`.
const PROBE_DIRS: &[(&str, InstallTarget)] = &[
    (".claude", InstallTarget::ClaudeCode),
    (".codex", InstallTarget::Codex),
    (".gemini", InstallTarget::GeminiCli),
    (".openclaw", InstallTarget::OpenClaw),
    (".hermes", InstallTarget::Hermes),
    (".config/opencode", InstallTarget::OpenCode),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallTarget {
    ClaudeCode,
    Codex,
    GeminiCli,
    #[value(name = "openclaw")]
    OpenClaw,
    #[value(name = "hermes")]
    Hermes,
    #[value(name = "opencode")]
    OpenCode,
}

impl InstallTarget {
    fn display(self) -> &'static str {
        match self {
            InstallTarget::ClaudeCode => "Claude Code",
            InstallTarget::Codex => "Codex",
            InstallTarget::GeminiCli => "Gemini CLI",
            InstallTarget::OpenClaw => "OpenClaw",
            InstallTarget::Hermes => "Hermes",
            InstallTarget::OpenCode => "OpenCode",
        }
    }
}

/// Entrypoint for `memex install`. `targets` empty = auto-detect from `home`.
pub fn run(home: &Path, targets: &[InstallTarget], dry_run: bool) -> Result<()> {
    let selected: Vec<InstallTarget> = if targets.is_empty() {
        detect(home)
    } else {
        targets.to_vec()
    };

    if selected.is_empty() {
        let probed: Vec<String> = PROBE_DIRS
            .iter()
            .map(|(d, _)| format!("{}/{d}", home.display()))
            .collect();
        eprintln!("no agent CLIs detected (looked for {})", probed.join(", "));
        eprintln!(
            "install at least one of: Claude Code, Codex, Gemini CLI, OpenClaw, Hermes, OpenCode, then retry"
        );
        bail!("no agents detected");
    }

    for target in &selected {
        if let Some(spec) = file_agent_spec(*target) {
            install_file_agent(spec, home, dry_run)?;
            continue;
        }
        match target {
            InstallTarget::OpenClaw => install_openclaw(home, dry_run)?,
            InstallTarget::Hermes => install_hermes(home, dry_run)?,
            InstallTarget::OpenCode => install_opencode(home, dry_run)?,
            _ => unreachable!("file_agent_spec covers all other variants"),
        }
    }

    if !dry_run {
        println!();
        println!("done. Sessions will auto-ingest and the daemon will pre-warm on session start.");
        println!("verify: memex doctor");
    }
    Ok(())
}

fn detect(home: &Path) -> Vec<InstallTarget> {
    PROBE_DIRS
        .iter()
        .filter(|(d, _)| home.join(d).is_dir())
        .map(|(_, t)| *t)
        .collect()
}

/// File-based agent: hooks live in a settings JSON file and skills under
/// `<agent_dir>/skills/`. Codex/Claude Code/Gemini CLI all match this shape.
struct FileAgentSpec {
    target: InstallTarget,
    agent_dir: &'static str,
    hooks_file: &'static str,
    template: &'static str,
    events: &'static [&'static str],
}

const FILE_AGENTS: &[FileAgentSpec] = &[
    FileAgentSpec {
        target: InstallTarget::ClaudeCode,
        agent_dir: ".claude",
        hooks_file: "settings.json",
        template: CLAUDE_CODE_HOOKS,
        events: &["SessionStart", "SessionEnd"],
    },
    FileAgentSpec {
        target: InstallTarget::Codex,
        agent_dir: ".codex",
        hooks_file: "hooks.json",
        template: CODEX_HOOKS,
        events: &["SessionStart", "Stop"],
    },
    FileAgentSpec {
        target: InstallTarget::GeminiCli,
        agent_dir: ".gemini",
        hooks_file: "settings.json",
        template: GEMINI_CLI_HOOKS,
        events: &["SessionStart", "SessionEnd"],
    },
];

fn file_agent_spec(target: InstallTarget) -> Option<&'static FileAgentSpec> {
    FILE_AGENTS.iter().find(|s| s.target == target)
}

fn install_file_agent(spec: &FileAgentSpec, home: &Path, dry_run: bool) -> Result<()> {
    let target = home.join(spec.agent_dir).join(spec.hooks_file);
    merge_hooks(&target, spec.template, spec.events, dry_run)?;
    println!("✓ {}: hooks → {}", spec.target.display(), target.display());
    install_skills(&home.join(spec.agent_dir).join("skills"), dry_run)?;
    Ok(())
}

/// OpenClaw skills land in `~/.agents/skills/` (its `agents-skills-personal`
/// source root). The hook pack is `~/.openclaw/hook-packs/memex/` linked via
/// `openclaw plugins install --link`.
fn install_openclaw(home: &Path, dry_run: bool) -> Result<()> {
    println!(
        "✓ {}: skills + hook pack",
        InstallTarget::OpenClaw.display()
    );
    install_skills(&home.join(".agents").join("skills"), dry_run)?;
    install_openclaw_hook_pack(home, dry_run)?;
    Ok(())
}

fn install_openclaw_hook_pack(home: &Path, dry_run: bool) -> Result<()> {
    let pack_dir = home.join(".openclaw").join("hook-packs").join("memex");
    let hook_dir = pack_dir.join("memex");
    if dry_run {
        println!("  would write hook pack: {}", pack_dir.display());
        println!(
            "  would run: openclaw plugins install --link {}",
            pack_dir.display()
        );
        return Ok(());
    }
    fs::create_dir_all(&hook_dir).with_context(|| format!("create {}", hook_dir.display()))?;
    fs::write(pack_dir.join("package.json"), OPENCLAW_PACKAGE_JSON)
        .with_context(|| format!("write {}/package.json", pack_dir.display()))?;
    fs::write(hook_dir.join("HOOK.md"), OPENCLAW_HOOK_MD)
        .with_context(|| format!("write {}/HOOK.md", hook_dir.display()))?;
    fs::write(hook_dir.join("handler.js"), OPENCLAW_HANDLER_JS)
        .with_context(|| format!("write {}/handler.js", hook_dir.display()))?;
    println!("  hook pack → {}", pack_dir.display());

    // Tests set MEMEX_SKIP_OPENCLAW_REGISTER=1 — the openclaw CLI doesn't
    // honor a HOME override, so without this it would write tempdir paths
    // into the real `~/.openclaw/openclaw.json`.
    if std::env::var("MEMEX_SKIP_OPENCLAW_REGISTER").as_deref() == Ok("1") {
        return Ok(());
    }
    let result = std::process::Command::new("openclaw")
        .args(["plugins", "install", "--link"])
        .arg(&pack_dir)
        .output();
    match result {
        Ok(out) if out.status.success() => {
            println!("  hook registered with openclaw (restart the gateway to load)");
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let detail = err
                .lines()
                .find(|l| l.contains("Error") || l.contains("fail"))
                .unwrap_or_else(|| err.trim().lines().last().unwrap_or("(no detail)"));
            eprintln!("  warning: openclaw plugins install failed: {detail}");
            eprintln!(
                "  to retry: openclaw plugins install --link {}",
                pack_dir.display()
            );
        }
        Err(_) => {
            eprintln!("  note: `openclaw` not on PATH — skipped plugin registration.");
            eprintln!(
                "        to enable later: openclaw plugins install --link {}",
                pack_dir.display()
            );
        }
    }
    Ok(())
}

/// Hermes shares the `~/.agents/skills/` skill root with OpenClaw. Its hook
/// loader auto-discovers any subdir of `~/.hermes/hooks/` containing
/// `HOOK.yaml` + `handler.py`, so install is just two file writes.
fn install_hermes(home: &Path, dry_run: bool) -> Result<()> {
    println!("✓ {}: skills + hook", InstallTarget::Hermes.display());
    install_skills(&home.join(".agents").join("skills"), dry_run)?;
    install_hermes_hook(home, dry_run)?;
    Ok(())
}

fn install_hermes_hook(home: &Path, dry_run: bool) -> Result<()> {
    let hook_dir = home.join(".hermes").join("hooks").join("memex");
    if dry_run {
        println!("  would write hook: {}", hook_dir.display());
        return Ok(());
    }
    fs::create_dir_all(&hook_dir).with_context(|| format!("create {}", hook_dir.display()))?;
    fs::write(hook_dir.join("HOOK.yaml"), HERMES_HOOK_YAML)
        .with_context(|| format!("write {}/HOOK.yaml", hook_dir.display()))?;
    fs::write(hook_dir.join("handler.py"), HERMES_HANDLER_PY)
        .with_context(|| format!("write {}/handler.py", hook_dir.display()))?;
    println!(
        "  hook → {} (restart `hermes gateway` to load)",
        hook_dir.display()
    );
    Ok(())
}

/// OpenCode auto-loads any `.ts`/`.js` file under `~/.config/opencode/plugins/`,
/// so install is a single file drop. Skills live alongside under
/// `~/.config/opencode/skills/`. Restart any open opencode session for the
/// plugin to take effect.
fn install_opencode(home: &Path, dry_run: bool) -> Result<()> {
    println!("✓ {}: skills + plugin", InstallTarget::OpenCode.display());
    let plugins_dir = home.join(".config").join("opencode").join("plugins");
    let plugin_file = plugins_dir.join("memex.ts");
    if dry_run {
        println!("  would write plugin: {}", plugin_file.display());
    } else {
        fs::create_dir_all(&plugins_dir)
            .with_context(|| format!("create {}", plugins_dir.display()))?;
        fs::write(&plugin_file, OPENCODE_PLUGIN_TS)
            .with_context(|| format!("write {}", plugin_file.display()))?;
        println!(
            "  plugin → {} (restart opencode to load)",
            plugin_file.display()
        );
    }
    install_skills(
        &home.join(".config").join("opencode").join("skills"),
        dry_run,
    )?;
    Ok(())
}

fn install_skills(skills_dir: &Path, dry_run: bool) -> Result<()> {
    for (name, body) in SKILLS {
        let dir = skills_dir.join(name);
        let file = dir.join("SKILL.md");
        if dry_run {
            println!("  would write skill: {}", file.display());
            continue;
        }
        fs::create_dir_all(&dir).with_context(|| format!("create skill dir {}", dir.display()))?;
        fs::write(&file, body).with_context(|| format!("write skill {}", file.display()))?;
    }
    if !dry_run {
        println!("  skills → {}", skills_dir.display());
    }
    Ok(())
}

/// Merge each named event from `src_json` into `target`, removing any existing
/// memex-managed entries first.
fn merge_hooks(target: &Path, src_json: &str, events: &[&str], dry_run: bool) -> Result<()> {
    let mut existing = read_json_or_empty(target)?;
    let src: Value = serde_json::from_str(src_json).context("parse embedded hook template")?;

    let existing_hooks = existing
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} root is not a JSON object", target.display()))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}.hooks is not an object", target.display()))?;

    for event in events {
        // Strip prior memex entries.
        if let Some(arr) = existing_hooks
            .get_mut(*event)
            .and_then(|v| v.as_array_mut())
        {
            arr.retain(|entry| !is_memex_managed(entry));
        }
        // Append fresh entry from the template.
        let Some(src_entry) = src
            .get("hooks")
            .and_then(|h| h.get(event))
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .cloned()
        else {
            continue;
        };
        let arr = existing_hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow!("hooks.{event} is not an array"))?;
        arr.push(src_entry);
        // If we just emptied the event by stripping memex entries, leave the
        // empty array — it's still valid and avoids reorder churn.
    }

    if dry_run {
        println!("would write {}:", target.display());
        println!("{}", serde_json::to_string_pretty(&existing)?);
        return Ok(());
    }

    write_pretty_json(target, &existing)
}

fn read_json_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parse {} as JSON", path.display()))
}

fn write_pretty_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value)? + "\n";
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// True when any nested `.hooks[].command` contains a memex sentinel.
fn is_memex_managed(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter().any(|inner| {
                inner
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| MEMEX_SENTINELS.iter().any(|s| c.contains(s)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Entrypoint for `memex uninstall`. `targets` empty = auto-detect from `home`.
/// Inverse of `run`: strips memex-managed hook entries and removes the three
/// Claude Code skill directories. Stops the daemon first so it releases its
/// socket. Leaves `~/.memex/` alone unless `purge` is true.
pub fn run_uninstall(
    home: &Path,
    targets: &[InstallTarget],
    dry_run: bool,
    purge: bool,
) -> Result<()> {
    let selected: Vec<InstallTarget> = if targets.is_empty() {
        detect(home)
    } else {
        targets.to_vec()
    };

    // Stop the daemon first. Idempotent: returns non-zero if it wasn't running,
    // which we ignore — the goal is just to release the socket and free RAM.
    if dry_run {
        println!("would stop daemon (if running)");
    } else {
        let _ = crate::daemon::stop();
    }

    if selected.is_empty() {
        let probed: Vec<String> = PROBE_DIRS
            .iter()
            .map(|(d, _)| format!("{}/{d}", home.display()))
            .collect();
        eprintln!("no agent CLIs detected (looked for {})", probed.join(", "));
        // Not an error — the daemon was still stopped above, and `--purge`
        // may still be the user's reason for running this.
    }

    for target in &selected {
        if let Some(spec) = file_agent_spec(*target) {
            uninstall_file_agent(spec, home, dry_run)?;
            continue;
        }
        match target {
            InstallTarget::OpenClaw => uninstall_openclaw(home, dry_run)?,
            InstallTarget::Hermes => uninstall_hermes(home, dry_run)?,
            InstallTarget::OpenCode => uninstall_opencode(home, dry_run)?,
            _ => unreachable!("file_agent_spec covers all other variants"),
        }
    }

    if purge {
        let memex_root = home.join(".memex");
        if memex_root.is_dir() {
            if dry_run {
                println!(
                    "would delete {} (wiki, models, daemon state)",
                    memex_root.display()
                );
            } else {
                fs::remove_dir_all(&memex_root)
                    .with_context(|| format!("remove {}", memex_root.display()))?;
                println!("✓ purged {}", memex_root.display());
            }
        }
    }

    if !dry_run {
        println!();
        println!("done. To finish removing the binary itself: npm uninstall -g @xiongzubiao/memex");
    }
    Ok(())
}

fn uninstall_file_agent(spec: &FileAgentSpec, home: &Path, dry_run: bool) -> Result<()> {
    let target = home.join(spec.agent_dir).join(spec.hooks_file);
    if strip_memex_hooks(&target, dry_run)? {
        println!("✓ {}: hooks ← {}", spec.target.display(), target.display());
    }
    remove_skills(&home.join(spec.agent_dir).join("skills"), dry_run)?;
    Ok(())
}

fn uninstall_openclaw(home: &Path, dry_run: bool) -> Result<()> {
    let skills_dir = home.join(".agents").join("skills");
    remove_skills(&skills_dir, dry_run)?;
    println!(
        "✓ {}: skills ← {}",
        InstallTarget::OpenClaw.display(),
        skills_dir.display()
    );
    uninstall_openclaw_hook_pack(home, dry_run);
    Ok(())
}

fn uninstall_hermes(home: &Path, dry_run: bool) -> Result<()> {
    let skills_dir = home.join(".agents").join("skills");
    remove_skills(&skills_dir, dry_run)?;
    println!(
        "✓ {}: skills ← {}",
        InstallTarget::Hermes.display(),
        skills_dir.display()
    );
    let hook_dir = home.join(".hermes").join("hooks").join("memex");
    if hook_dir.exists() {
        if dry_run {
            println!("  would remove: {}", hook_dir.display());
        } else {
            fs::remove_dir_all(&hook_dir)
                .with_context(|| format!("remove {}", hook_dir.display()))?;
            println!("  hook ← {}", hook_dir.display());
        }
    }
    Ok(())
}

fn uninstall_opencode(home: &Path, dry_run: bool) -> Result<()> {
    let oc_root = home.join(".config").join("opencode");
    let plugin_file = oc_root.join("plugins").join("memex.ts");
    let skills_dir = oc_root.join("skills");
    remove_skills(&skills_dir, dry_run)?;
    println!(
        "✓ {}: skills ← {}",
        InstallTarget::OpenCode.display(),
        skills_dir.display()
    );
    if plugin_file.exists() {
        if dry_run {
            println!("  would remove: {}", plugin_file.display());
        } else {
            fs::remove_file(&plugin_file)
                .with_context(|| format!("remove {}", plugin_file.display()))?;
            println!("  plugin ← {}", plugin_file.display());
        }
    }
    Ok(())
}

fn uninstall_openclaw_hook_pack(home: &Path, dry_run: bool) {
    let pack_dir = home.join(".openclaw").join("hook-packs").join("memex");
    if dry_run {
        println!("  would run: openclaw plugins uninstall memex");
        println!("  would remove: {}", pack_dir.display());
        return;
    }
    // Unregister with openclaw first so the link is dropped before we wipe
    // the directory it pointed at.
    if std::env::var("MEMEX_SKIP_OPENCLAW_REGISTER").as_deref() != Ok("1") {
        let _ = std::process::Command::new("openclaw")
            .args(["plugins", "uninstall", "memex"])
            .output();
    }
    if pack_dir.exists() {
        let _ = fs::remove_dir_all(&pack_dir);
        println!("  hook pack ← {}", pack_dir.display());
    }
}

fn remove_skills(skills_dir: &Path, dry_run: bool) -> Result<()> {
    let mut removed_any = false;
    for (name, _) in SKILLS {
        let dir = skills_dir.join(name);
        if !dir.exists() {
            continue;
        }
        if dry_run {
            println!("  would remove skill: {}", dir.display());
            removed_any = true;
            continue;
        }
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        removed_any = true;
    }
    if removed_any && !dry_run {
        println!("  skills ← {}", skills_dir.display());
    }
    Ok(())
}

/// Remove memex-managed entries from every event under `.hooks` in `target`.
/// Prunes events that become empty arrays, and removes the `hooks` key
/// entirely if no events remain. Returns true when the file existed and we
/// inspected it (regardless of whether anything was stripped).
fn strip_memex_hooks(target: &Path, dry_run: bool) -> Result<bool> {
    if !target.exists() {
        return Ok(false);
    }
    let mut value = read_json_or_empty(target)?;

    let Some(root) = value.as_object_mut() else {
        return Ok(true); // file existed but wasn't a JSON object — leave alone
    };
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(true); // no .hooks section, nothing to strip
    };

    for event in hooks.keys().cloned().collect::<Vec<_>>() {
        if let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) {
            arr.retain(|entry| !is_memex_managed(entry));
            if arr.is_empty() {
                hooks.remove(&event);
            }
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }

    if dry_run {
        println!("would rewrite {}:", target.display());
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(true);
    }

    write_pretty_json(target, &value)?;
    Ok(true)
}

/// Same lookup as the rest of memex: respect $HOME, fall back to dirs::home_dir.
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return Ok(PathBuf::from(h));
    }
    dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_home() -> TempDir {
        // Block tests from touching the real ~/.openclaw/openclaw.json through
        // the openclaw subprocess (which doesn't honor our $HOME override).
        // SAFETY: tests are single-threaded for env mutation; this env var is
        // process-wide for the test run but never unset (production code reads
        // it as falsy by default).
        unsafe {
            std::env::set_var("MEMEX_SKIP_OPENCLAW_REGISTER", "1");
        }
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".claude")).unwrap();
        t
    }

    #[test]
    fn install_claude_code_writes_hooks_and_skills() {
        let home = fake_home();
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();

        let hooks_file = home.path().join(".claude/settings.json");
        assert!(hooks_file.exists());
        let v: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("memex daemon"), "got {cmd}");
        let end_cmd = v["hooks"]["SessionEnd"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(end_cmd.contains("memex hook"), "got {end_cmd}");

        let skill = home.path().join(".claude/skills/memex-query/SKILL.md");
        assert!(skill.exists(), "skill not written: {}", skill.display());
    }

    #[test]
    fn install_codex_writes_hooks_and_skills() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".codex")).unwrap();
        run(t.path(), &[InstallTarget::Codex], false).unwrap();

        let hooks_file = t.path().join(".codex/hooks.json");
        assert!(hooks_file.exists());
        let v: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        // Codex uses Stop, not SessionEnd.
        let stop_cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            stop_cmd.contains("memex hook ingest codex"),
            "got {stop_cmd}"
        );

        for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
            let skill = t.path().join(".codex/skills").join(name).join("SKILL.md");
            assert!(
                skill.exists(),
                "codex skill not written: {}",
                skill.display()
            );
        }
    }

    #[test]
    fn install_gemini_cli_writes_hooks_and_skills() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".gemini")).unwrap();
        run(t.path(), &[InstallTarget::GeminiCli], false).unwrap();

        let settings_file = t.path().join(".gemini/settings.json");
        assert!(settings_file.exists());
        let v: Value = serde_json::from_str(&fs::read_to_string(&settings_file).unwrap()).unwrap();
        let end_cmd = v["hooks"]["SessionEnd"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            end_cmd.contains("memex hook ingest gemini-cli"),
            "got {end_cmd}"
        );

        for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
            let skill = t.path().join(".gemini/skills").join(name).join("SKILL.md");
            assert!(
                skill.exists(),
                "gemini skill not written: {}",
                skill.display()
            );
        }
    }

    #[test]
    fn install_openclaw_writes_skills_and_hook_pack() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".openclaw")).unwrap();
        run(t.path(), &[InstallTarget::OpenClaw], false).unwrap();

        // Skills land in ~/.agents/skills/ (OpenClaw's cross-agent location).
        for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
            let skill = t.path().join(".agents/skills").join(name).join("SKILL.md");
            assert!(
                skill.exists(),
                "openclaw skill not at expected path: {}",
                skill.display()
            );
        }

        // Hook pack written to ~/.openclaw/hook-packs/memex/ with the nested
        // layout OpenClaw's loader requires (extraDir iterates SUBDIRS).
        let pack = t.path().join(".openclaw/hook-packs/memex");
        assert!(pack.join("package.json").exists(), "missing package.json");
        assert!(pack.join("memex/HOOK.md").exists(), "missing memex/HOOK.md");
        assert!(
            pack.join("memex/handler.js").exists(),
            "missing memex/handler.js"
        );
        // The `openclaw plugins install --link` step is best-effort and only
        // tries `openclaw` on PATH; not asserted here (the binary may be
        // absent in the test sandbox).
    }

    #[test]
    fn detect_picks_up_openclaw() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".openclaw")).unwrap();
        let found = detect(t.path());
        assert_eq!(found, vec![InstallTarget::OpenClaw]);
    }

    #[test]
    fn install_hermes_writes_skills_and_hook() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".hermes")).unwrap();
        run(t.path(), &[InstallTarget::Hermes], false).unwrap();

        // Skills land in ~/.agents/skills/ (shared with OpenClaw).
        for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
            let skill = t.path().join(".agents/skills").join(name).join("SKILL.md");
            assert!(
                skill.exists(),
                "hermes skill not at expected path: {}",
                skill.display()
            );
        }

        // Hook files at ~/.hermes/hooks/memex/{HOOK.yaml,handler.py}.
        let hook = t.path().join(".hermes/hooks/memex");
        assert!(hook.join("HOOK.yaml").exists(), "missing HOOK.yaml");
        assert!(hook.join("handler.py").exists(), "missing handler.py");

        // Re-running should be idempotent (no panic, files still there).
        run(t.path(), &[InstallTarget::Hermes], false).unwrap();
        assert!(hook.join("HOOK.yaml").exists());
    }

    #[test]
    fn install_opencode_writes_plugin_and_skills() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".config/opencode")).unwrap();
        run(t.path(), &[InstallTarget::OpenCode], false).unwrap();

        let plugin = t.path().join(".config/opencode/plugins/memex.ts");
        assert!(plugin.exists(), "missing plugin file: {}", plugin.display());
        let body = fs::read_to_string(&plugin).unwrap();
        assert!(
            body.contains("session.idle"),
            "plugin body missing session.idle wiring"
        );
        assert!(body.contains("memex"), "plugin body missing memex command");

        for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
            let skill = t
                .path()
                .join(".config/opencode/skills")
                .join(name)
                .join("SKILL.md");
            assert!(
                skill.exists(),
                "opencode skill not written: {}",
                skill.display()
            );
        }

        // Idempotent: re-running just rewrites the same file.
        run(t.path(), &[InstallTarget::OpenCode], false).unwrap();
        assert!(plugin.exists());
    }

    #[test]
    fn detect_picks_up_opencode() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".config/opencode")).unwrap();
        let found = detect(t.path());
        assert_eq!(found, vec![InstallTarget::OpenCode]);
    }

    #[test]
    fn uninstall_opencode_removes_plugin_and_skills() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".config/opencode")).unwrap();
        run(t.path(), &[InstallTarget::OpenCode], false).unwrap();
        let plugin = t.path().join(".config/opencode/plugins/memex.ts");
        let skill = t
            .path()
            .join(".config/opencode/skills/memex-query/SKILL.md");
        assert!(plugin.exists() && skill.exists());

        run_uninstall(t.path(), &[InstallTarget::OpenCode], false, false).unwrap();
        assert!(!plugin.exists(), "plugin not removed");
        assert!(!skill.exists(), "skill not removed");
    }

    #[test]
    fn detect_picks_up_hermes() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".hermes")).unwrap();
        let found = detect(t.path());
        assert_eq!(found, vec![InstallTarget::Hermes]);
    }

    #[test]
    fn uninstall_hermes_removes_hook_dir() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".hermes")).unwrap();
        run(t.path(), &[InstallTarget::Hermes], false).unwrap();
        let hook = t.path().join(".hermes/hooks/memex");
        assert!(hook.exists());
        run_uninstall(t.path(), &[InstallTarget::Hermes], false, false).unwrap();
        assert!(!hook.exists(), "hook dir should be removed");
    }

    #[test]
    fn uninstall_removes_skills_for_all_agents() {
        let t = tempfile::tempdir().unwrap();
        for d in [".claude", ".codex", ".gemini"] {
            fs::create_dir_all(t.path().join(d)).unwrap();
        }
        run(t.path(), &[], false).unwrap();
        // All three skill dirs populated.
        for (d, _) in [(".claude", ""), (".codex", ""), (".gemini", "")] {
            for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
                let p = t.path().join(d).join("skills").join(name).join("SKILL.md");
                assert!(p.exists(), "missing: {}", p.display());
            }
        }
        run_uninstall(t.path(), &[], false, false).unwrap();
        // All three gone.
        for d in [".claude", ".codex", ".gemini"] {
            for name in ["memex-query", "memex-ingest", "memex-brainstorm"] {
                let p = t.path().join(d).join("skills").join(name);
                assert!(!p.exists(), "should be removed: {}", p.display());
            }
        }
    }

    #[test]
    fn install_is_idempotent_and_replaces_stale_entries() {
        let home = fake_home();

        // Seed an old-style memex entry that should get replaced.
        let hooks_file = home.path().join(".claude/settings.json");
        let stale = json!({
            "hooks": {
                "SessionEnd": [
                    {
                        "matcher": "*",
                        "hooks": [
                            {"type": "command", "command": "memex ingest --agent claude-code OLD"}
                        ]
                    },
                    {
                        "matcher": "*",
                        "hooks": [{"type": "command", "command": "unrelated-user-hook"}]
                    }
                ]
            }
        });
        fs::write(&hooks_file, serde_json::to_string_pretty(&stale).unwrap()).unwrap();

        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();

        let v: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        let session_end = v["hooks"]["SessionEnd"].as_array().unwrap();
        // Stale memex entry removed, unrelated entry preserved, one fresh memex entry.
        assert_eq!(session_end.len(), 2, "got {session_end:?}");
        let memex_count = session_end.iter().filter(|e| is_memex_managed(e)).count();
        assert_eq!(memex_count, 1, "expected exactly one memex entry");

        // Second install — still exactly one.
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();
        let v2: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        let memex_count = v2["hooks"]["SessionEnd"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| is_memex_managed(e))
            .count();
        assert_eq!(memex_count, 1, "re-running install duplicated memex entry");
    }

    #[test]
    fn dry_run_does_not_write() {
        let home = fake_home();
        run(home.path(), &[InstallTarget::ClaudeCode], true).unwrap();
        assert!(!home.path().join(".claude/settings.json").exists());
        assert!(
            !home
                .path()
                .join(".claude/skills/memex-query/SKILL.md")
                .exists()
        );
    }

    #[test]
    fn detect_finds_existing_agent_dirs() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".gemini")).unwrap();
        let found = detect(t.path());
        assert_eq!(found, vec![InstallTarget::GeminiCli]);
    }

    #[test]
    fn no_agents_errors() {
        let t = tempfile::tempdir().unwrap();
        let err = run(t.path(), &[], false).unwrap_err();
        assert!(err.to_string().contains("no agents"), "got: {err}");
    }

    #[test]
    fn uninstall_strips_memex_hooks_preserves_user_entries() {
        let home = fake_home();
        // Install first to seed the state.
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();

        // Add an unrelated user-owned hook the uninstall must NOT touch.
        let hooks_file = home.path().join(".claude/settings.json");
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        let session_end = v["hooks"]["SessionEnd"].as_array_mut().unwrap();
        session_end.push(json!({
            "matcher": "*",
            "hooks": [{"type": "command", "command": "some-user-thing"}]
        }));
        fs::write(&hooks_file, serde_json::to_string_pretty(&v).unwrap()).unwrap();

        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();

        let v2: Value = serde_json::from_str(&fs::read_to_string(&hooks_file).unwrap()).unwrap();
        // SessionEnd has only the user entry left; SessionStart is gone entirely.
        assert!(v2["hooks"]["SessionStart"].is_null(), "got {v2}");
        let remaining = v2["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(remaining.len(), 1);
        let cmd = remaining[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "some-user-thing");

        // Skills removed.
        assert!(!home.path().join(".claude/skills/memex-query").exists());
        assert!(!home.path().join(".claude/skills/memex-ingest").exists());
        assert!(!home.path().join(".claude/skills/memex-brainstorm").exists());
    }

    #[test]
    fn uninstall_removes_hooks_key_when_empty() {
        let home = fake_home();
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();
        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();

        let v: Value = serde_json::from_str(
            &fs::read_to_string(home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        // No user-owned entries existed, so .hooks should be gone (not an empty object).
        assert!(v.get("hooks").is_none(), "expected no .hooks key, got {v}");
    }

    #[test]
    fn uninstall_is_idempotent() {
        let home = fake_home();
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();
        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();
        // Second run on already-clean state should not error.
        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();
    }

    #[test]
    fn uninstall_dry_run_does_not_write() {
        let home = fake_home();
        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();
        let before = fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], true, false).unwrap();
        let after = fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
        assert_eq!(before, after, "dry-run should not have rewritten hooks");
        assert!(home.path().join(".claude/skills/memex-query").exists());
    }

    #[test]
    fn install_preserves_unrelated_settings_keys() {
        let home = fake_home();
        let settings = home.path().join(".claude/settings.json");
        let seeded = json!({
            "permissions": {"defaultMode": "auto", "allow": ["Bash(git *)"]},
            "enabledPlugins": {"superpowers@official": true},
            "statusLine": {"type": "command", "command": "/bin/echo hi"}
        });
        fs::write(&settings, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();

        run(home.path(), &[InstallTarget::ClaudeCode], false).unwrap();

        let v: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["permissions"], seeded["permissions"]);
        assert_eq!(v["enabledPlugins"], seeded["enabledPlugins"]);
        assert_eq!(v["statusLine"], seeded["statusLine"]);
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("memex daemon"));

        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();

        let v: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["permissions"], seeded["permissions"]);
        assert_eq!(v["enabledPlugins"], seeded["enabledPlugins"]);
        assert_eq!(v["statusLine"], seeded["statusLine"]);
        assert!(
            v.get("hooks").is_none(),
            "hooks should be stripped, got {v}"
        );
    }

    #[test]
    fn uninstall_purge_deletes_memex_root() {
        let home = fake_home();
        let memex_root = home.path().join(".memex");
        fs::create_dir_all(memex_root.join("wiki")).unwrap();
        fs::write(memex_root.join("wiki/test.md"), "data").unwrap();

        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, true).unwrap();

        assert!(!memex_root.exists(), "expected ~/.memex to be gone");
    }

    #[test]
    fn uninstall_without_purge_keeps_memex_root() {
        let home = fake_home();
        let memex_root = home.path().join(".memex");
        fs::create_dir_all(memex_root.join("wiki")).unwrap();
        fs::write(memex_root.join("wiki/test.md"), "data").unwrap();

        run_uninstall(home.path(), &[InstallTarget::ClaudeCode], false, false).unwrap();

        assert!(
            memex_root.join("wiki/test.md").exists(),
            "wiki must survive without --purge"
        );
    }
}
