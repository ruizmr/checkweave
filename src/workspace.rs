//! Workspace discovery and idempotent initialization.
//!
//! The root is the canonical Git worktree from `git rev-parse --show-toplevel`
//! when the path is inside a repository. Linked worktrees stay distinct.
//! Otherwise the root is the nearest ancestor that already has Checkweave state,
//! or the given directory. Initialization never creates state in some other ancestor.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};
use fs2::FileExt;
use serde_json::{Value, json};

const WORKSPACE_VERSION: u64 = 1;
const STATE_DIR_NAME: &str = ".checkweave";
const MARKER_FILE: &str = "workspace.json";
const INIT_LOCK_FILE: &str = "init.lock";
const MANAGED_RULE: &str = "checkweave.mdc";

const MANAGED_RULE_BODY: &str = r#"---
description: Check JSONL collections with Checkweave and inspect evidence by id
alwaysApply: true
---

When a task depends on records in JSONL files, use Checkweave instead of scanning the collection by hand.

<!-- checkweave:managed -->

Call the collection check with workspace-relative globs and a predicate. Each predicate `path` is a JSON Pointer (RFC 6901). The empty string is the whole record. `/status`, `/user/id`, and `/items/0/name` are members. `/` is the empty-name member, not the record. `~0` and `~1` escape `~` and `/`.

Read coverage before the sample. `evaluated`, `matched`, `unmatched`, `unresolved`, and `skipped` describe processing coverage only. They are not a measure of classification accuracy.

`matched: true` or `false` is a direct observation of that record against the predicate. `matched: null` means the record was unresolved (invalid JSON or a value the predicate cannot compare). Use `reason` and `source` (path, line, fingerprint) to explain it.

Use evidence with the report `id` to open the retained result. If the response says the evidence expired or was not found, say that. Do not treat a missing handle as a pass, and do not invent records.

Trust scope is the fingerprinted snapshot in `sources` plus the predicate you submitted. A result does not cover files outside `include`, fields the predicate did not read, or edits made after the check. `freshness: validated` means that snapshot, not that the workspace is frozen. Do not present evidence from an older check as the current workspace; run a check again.

Prefer the check and evidence tools over copying large collections into the conversation.
"#;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub state_dir: PathBuf,
}

impl Workspace {
    pub fn discover(path: &Path) -> anyhow::Result<Self> {
        let start = normalize_start(path)?;
        Ok(Self::at(select_root(&start)?))
    }

    pub fn initialize(path: &Path, integration: &str) -> anyhow::Result<Value> {
        match integration {
            "cursor" | "none" => {}
            other => bail!("unsupported integration `{other}`"),
        }
        let start = normalize_start(path)?;
        let root = select_root(&start)?;
        if root != start && !start.starts_with(&root) {
            bail!(
                "refusing to initialize unrelated ancestor {}",
                root.display()
            );
        }

        let state_dir = root.join(STATE_DIR_NAME);
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let lock_path = state_dir.join(INIT_LOCK_FILE);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock_file
            .lock_exclusive()
            .context("acquire workspace init lock")?;

        if let Some(version) = existing_marker_version(&root)?
            && version > WORKSPACE_VERSION
        {
            bail!("unsupported workspace version {version}");
        }

        let marker = marker_value(&root)?;
        let gitignore = prepare_gitignore(&root)?;
        let mcp = if integration == "cursor" {
            prepare_mcp(&root)?
        } else {
            None
        };
        let rule = if integration == "cursor" {
            plan_rule(&root)?
        } else {
            RulePlan::Skip
        };

        write_if_changed(&state_dir.join(MARKER_FILE), &marker)?;
        write_if_changed(&state_dir.join(".gitignore"), b"*\n")?;
        if let Some(bytes) = gitignore {
            write_if_changed(&root.join(".gitignore"), &bytes)?;
        }
        if let Some(bytes) = mcp {
            let cursor_dir = root.join(".cursor");
            fs::create_dir_all(&cursor_dir)?;
            write_if_changed(&cursor_dir.join("mcp.json"), &bytes)?;
        }
        let mut rule_conflict = None;
        match rule {
            RulePlan::Write(bytes) => {
                let rules_dir = root.join(".cursor").join("rules");
                fs::create_dir_all(&rules_dir)?;
                write_if_changed(&rules_dir.join(MANAGED_RULE), &bytes)?;
            }
            RulePlan::Preserve => {
                rule_conflict = Some(
                    "left existing .cursor/rules/checkweave.mdc in place; it has no checkweave:managed marker",
                );
            }
            RulePlan::Skip => {}
        }

        drop(lock_file);
        let mut value = json!({
            "root": path_string(&root)?,
            "state_dir": path_string(&state_dir)?,
            "integration": integration,
        });
        if let Some(message) = rule_conflict {
            value["rule"] = json!(message);
        }
        Ok(value)
    }

    /// Remove the managed Cursor MCP entry and managed rule file.
    /// Custom rules, unrelated MCP servers, gitignore text, and `.checkweave` stay.
    pub fn remove_integration(path: &Path) -> anyhow::Result<Value> {
        let start = normalize_start(path)?;
        let root = select_root(&start)?;
        let state_dir = root.join(STATE_DIR_NAME);
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let lock_path = state_dir.join(INIT_LOCK_FILE);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock_file
            .lock_exclusive()
            .context("acquire workspace init lock")?;

        let mut removed = Vec::new();
        let mut preserved = Vec::new();
        match remove_managed_mcp(&root)? {
            McpRemoval::Removed => removed.push("mcpServers.checkweave".to_string()),
            McpRemoval::Absent => {}
            McpRemoval::Preserved(reason) => preserved.push(reason),
        }
        match remove_managed_rule(&root)? {
            RuleRemoval::Removed => removed.push(".cursor/rules/checkweave.mdc".to_string()),
            RuleRemoval::Absent => {}
            RuleRemoval::Preserved(reason) => preserved.push(reason),
        }
        drop(lock_file);
        Ok(json!({
            "root": path_string(&root)?,
            "removed": removed,
            "preserved": preserved,
            "state": "kept",
        }))
    }

    fn at(root: PathBuf) -> Self {
        let state_dir = root.join(STATE_DIR_NAME);
        Self { root, state_dir }
    }
}

fn select_root(start: &Path) -> anyhow::Result<PathBuf> {
    if let Some(git_root) = git_toplevel(start)? {
        if git_root != *start && !start.starts_with(&git_root) {
            bail!("refusing to use unrelated git root {}", git_root.display());
        }
        return Ok(git_root);
    }
    if let Some(existing) = nearest_initialized_ancestor(start)? {
        return Ok(existing);
    }
    Ok(start.to_path_buf())
}

fn normalize_start(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.exists() {
        bail!("workspace path does not exist: {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    if canonical.is_file() {
        return canonical
            .parent()
            .map(Path::to_path_buf)
            .context("file has no parent directory");
    }
    Ok(canonical)
}

fn git_toplevel(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("run git rev-parse --show-toplevel"),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(output.stdout).context("git toplevel is not utf-8")?;
    let printed = text.trim();
    if printed.is_empty() {
        return Ok(None);
    }
    let root = PathBuf::from(printed);
    if !root.exists() {
        bail!("git toplevel does not exist: {}", root.display());
    }
    Ok(Some(root.canonicalize().with_context(|| {
        format!("canonicalize git toplevel {}", root.display())
    })?))
}

fn nearest_initialized_ancestor(start: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if initialized_root(dir)?.is_some() {
            return Ok(Some(dir.to_path_buf()));
        }
        current = dir.parent();
    }
    Ok(None)
}

fn initialized_root(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let marker_path = dir.join(STATE_DIR_NAME).join(MARKER_FILE);
    if !marker_path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&marker_path)
        .with_context(|| format!("read {}", marker_path.display()))?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Some(root) = value.get("root").and_then(Value::as_str) else {
        return Ok(None);
    };
    let root_path = PathBuf::from(root);
    let canonical = if root_path.exists() {
        root_path.canonicalize().unwrap_or(root_path)
    } else {
        root_path
    };
    if canonical == dir {
        Ok(Some(canonical))
    } else {
        Ok(None)
    }
}

fn existing_marker_version(root: &Path) -> anyhow::Result<Option<u64>> {
    let marker_path = root.join(STATE_DIR_NAME).join(MARKER_FILE);
    if !marker_path.is_file() {
        return Ok(None);
    }
    let text = match fs::read_to_string(&marker_path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    match value.get("version").and_then(Value::as_u64) {
        Some(version) => Ok(Some(version)),
        None => Ok(None),
    }
}

fn marker_value(root: &Path) -> anyhow::Result<Vec<u8>> {
    let value = json!({
        "version": WORKSPACE_VERSION,
        "root": path_string(root)?,
    });
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn prepare_gitignore(root: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let path = root.join(".gitignore");
    let existing = if path.is_file() {
        fs::read(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        Vec::new()
    };
    if state_is_effectively_ignored(root, &existing)? {
        return Ok(None);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with(b"\n") {
        next.push(b'\n');
    }
    next.extend_from_slice(b".checkweave/\n");
    Ok(Some(next))
}

fn state_is_effectively_ignored(root: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Ok(false);
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    for line in text.lines() {
        if let Err(error) = builder.add_line(None, line) {
            bail!("invalid gitignore rule: {error}");
        }
    }
    let matcher = builder
        .build()
        .map_err(|error| anyhow!("gitignore rules are not usable: {error}"))?;
    Ok(matcher.matched(".checkweave", true).is_ignore())
}

enum McpRemoval {
    Removed,
    Absent,
    Preserved(String),
}

enum RuleRemoval {
    Removed,
    Absent,
    Preserved(String),
}

fn remove_managed_mcp(root: &Path) -> anyhow::Result<McpRemoval> {
    let path = root.join(".cursor").join("mcp.json");
    if !path.exists() {
        return Ok(McpRemoval::Absent);
    }
    if !path.is_file() {
        bail!("conflicting .cursor/mcp.json is not a file");
    }
    let original = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let text = std::str::from_utf8(&original).context("malformed .cursor/mcp.json: not utf-8")?;
    let mut document: Value = serde_json::from_str(text).context("malformed .cursor/mcp.json")?;
    let Some(servers) = document
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
    else {
        return Ok(McpRemoval::Absent);
    };
    let Some(current) = servers.get("checkweave").cloned() else {
        return Ok(McpRemoval::Absent);
    };
    let managed = current
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| Path::new(command).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "checkweave" || name == "checkweave.exe");
    if !managed {
        return Ok(McpRemoval::Preserved(
            "left mcpServers.checkweave because its command is not checkweave".into(),
        ));
    }
    servers.remove("checkweave");
    write_atomic(&path, &pretty_json(&document)?)?;
    Ok(McpRemoval::Removed)
}

fn remove_managed_rule(root: &Path) -> anyhow::Result<RuleRemoval> {
    let path = root.join(".cursor").join("rules").join(MANAGED_RULE);
    if !path.exists() {
        return Ok(RuleRemoval::Absent);
    }
    if !path.is_file() {
        bail!("conflicting .cursor/rules/{MANAGED_RULE} is not a file");
    }
    let existing = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let managed = existing
        .windows(b"checkweave:managed".len())
        .any(|window| window == b"checkweave:managed");
    if !managed {
        return Ok(RuleRemoval::Preserved(
            "left .cursor/rules/checkweave.mdc because it has no checkweave:managed marker".into(),
        ));
    }
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    Ok(RuleRemoval::Removed)
}

enum RulePlan {
    Write(Vec<u8>),
    Preserve,
    Skip,
}

fn plan_rule(root: &Path) -> anyhow::Result<RulePlan> {
    let path = root.join(".cursor").join("rules").join(MANAGED_RULE);
    let desired = MANAGED_RULE_BODY.as_bytes();
    if !path.exists() {
        return Ok(RulePlan::Write(desired.to_vec()));
    }
    if !path.is_file() {
        bail!("conflicting .cursor/rules/{MANAGED_RULE} is not a file");
    }
    let existing = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if existing == desired {
        return Ok(RulePlan::Skip);
    }
    if existing
        .windows(b"checkweave:managed".len())
        .any(|window| window == b"checkweave:managed")
    {
        return Ok(RulePlan::Write(desired.to_vec()));
    }
    Ok(RulePlan::Preserve)
}

fn prepare_mcp(root: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let path = root.join(".cursor").join("mcp.json");
    let exe = std::env::current_exe()
        .context("resolve current executable")?
        .canonicalize()
        .context("canonicalize current executable")?;
    let desired = json!({
        "command": path_string(&exe)?,
        "args": ["--workspace", path_string(root)?, "mcp"],
    });

    if !path.exists() {
        let document = json!({ "mcpServers": { "checkweave": desired } });
        return Ok(Some(pretty_json(&document)?));
    }
    if !path.is_file() {
        bail!("conflicting .cursor/mcp.json is not a file");
    }
    let original = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let text = std::str::from_utf8(&original).context("malformed .cursor/mcp.json: not utf-8")?;
    let mut document: Value = serde_json::from_str(text).context("malformed .cursor/mcp.json")?;
    let Some(root_obj) = document.as_object_mut() else {
        bail!("malformed .cursor/mcp.json: root must be an object");
    };
    let servers = root_obj.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers_obj) = servers.as_object_mut() else {
        bail!("conflicting .cursor/mcp.json: mcpServers must be an object");
    };
    if let Some(current) = servers_obj.get("checkweave") {
        ensure_managed_checkweave(current, &desired)?;
        if current == &desired {
            return Ok(None);
        }
    }
    servers_obj.insert("checkweave".to_string(), desired);
    Ok(Some(pretty_json(&document)?))
}

fn ensure_managed_checkweave(current: &Value, desired: &Value) -> anyhow::Result<()> {
    let Some(obj) = current.as_object() else {
        bail!("conflicting mcpServers.checkweave: expected an object");
    };
    for key in obj.keys() {
        if key != "command" && key != "args" {
            bail!("conflicting mcpServers.checkweave field `{key}`");
        }
    }
    let Some(command) = obj.get("command").and_then(Value::as_str) else {
        bail!("conflicting mcpServers.checkweave: command must be a string");
    };
    let Some(args) = obj.get("args").and_then(Value::as_array) else {
        bail!("conflicting mcpServers.checkweave: args must be an array");
    };
    if args.iter().any(|arg| !arg.is_string()) {
        bail!("conflicting mcpServers.checkweave: args must be strings");
    }
    let file_name = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let desired_command = desired.get("command").and_then(Value::as_str).unwrap_or("");
    let managed =
        file_name == "checkweave" || file_name == "checkweave.exe" || command == desired_command;
    if !managed {
        bail!("conflicting mcpServers.checkweave command `{command}`");
    }
    Ok(())
}

fn pretty_json(value: &Value) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if path.is_file() && fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    write_atomic(path, bytes)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent for {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temp file in {}", parent.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        Err(error) => {
            error.file.persist(path).map(|_| ()).map_err(|retry| {
                anyhow!("atomic write to {} failed: {}", path.display(), retry.error)
            })
        }
    }
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("path is not utf-8: {}", path.display()))
}
