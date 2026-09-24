use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use checkweave::workspace::Workspace;

fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("checkweave-ws-{nanos}-{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        Self(scratch())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "checkweave")
        .env("GIT_AUTHOR_EMAIL", "checkweave@example.com")
        .env("GIT_COMMITTER_NAME", "checkweave")
        .env("GIT_COMMITTER_EMAIL", "checkweave@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn preserves_unrelated_files_and_is_idempotent() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::create_dir_all(root.join(".cursor/rules")).unwrap();
    let other_rule = b"---\ndescription: keep me\n---\nLeave this rule alone.\n";
    fs::write(root.join(".cursor/rules/other.mdc"), other_rule).unwrap();
    fs::write(
        root.join(".cursor/mcp.json"),
        "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"other\",\n      \"args\": [\"stay\"]\n    }\n  },\n  \"extra\": true\n}\n",
    )
    .unwrap();

    let first = Workspace::initialize(root, "cursor").unwrap();
    let root_text = first["root"].as_str().unwrap().to_string();
    assert_eq!(
        Workspace::discover(root).unwrap().root,
        PathBuf::from(&root_text)
    );
    let gitignore = read(&root.join(".gitignore"));
    assert!(gitignore.starts_with("target/\n*.log\n"));
    assert_eq!(gitignore.matches(".checkweave/").count(), 1);
    let mcp_after = read(&root.join(".cursor/mcp.json"));
    let mcp: serde_json::Value = serde_json::from_str(&mcp_after).unwrap();
    assert_eq!(mcp["extra"], true);
    assert_eq!(mcp["mcpServers"]["other"]["command"], "other");
    assert_eq!(mcp["mcpServers"]["other"]["args"][0], "stay");
    let command = mcp["mcpServers"]["checkweave"]["command"].as_str().unwrap();
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    assert_eq!(Path::new(command), exe);
    assert!(Path::new(command).is_absolute());
    assert_eq!(
        mcp["mcpServers"]["checkweave"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["--workspace", root_text.as_str(), "mcp"]
    );
    let rule = read(&root.join(".cursor/rules/checkweave.mdc"));
    assert!(rule.contains("JSON Pointer"));
    assert!(rule.contains("evidence"));
    assert!(rule.to_lowercase().contains("trust scope"));
    assert_eq!(
        fs::read(root.join(".cursor/rules/other.mdc")).unwrap(),
        other_rule
    );

    let second_bytes = (
        fs::read(root.join(".gitignore")).unwrap(),
        fs::read(root.join(".cursor/mcp.json")).unwrap(),
        fs::read(root.join(".cursor/rules/checkweave.mdc")).unwrap(),
        fs::read(root.join(".cursor/rules/other.mdc")).unwrap(),
    );
    Workspace::initialize(root, "cursor").unwrap();
    assert_eq!(fs::read(root.join(".gitignore")).unwrap(), second_bytes.0);
    assert_eq!(
        fs::read(root.join(".cursor/mcp.json")).unwrap(),
        second_bytes.1
    );
    assert_eq!(
        fs::read(root.join(".cursor/rules/checkweave.mdc")).unwrap(),
        second_bytes.2
    );
    assert_eq!(
        fs::read(root.join(".cursor/rules/other.mdc")).unwrap(),
        second_bytes.3
    );
    assert_eq!(gitignore.matches(".checkweave/").count(), 1);
}

#[test]
fn malformed_mcp_json_is_not_replaced() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::write(root.join(".gitignore"), "keep-me\n").unwrap();
    fs::create_dir_all(root.join(".cursor")).unwrap();
    let broken = b"{ this is not json\n";
    fs::write(root.join(".cursor/mcp.json"), broken).unwrap();

    let error = Workspace::initialize(root, "cursor").unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("malformed"), "unexpected error: {message}");
    assert_eq!(fs::read(root.join(".cursor/mcp.json")).unwrap(), broken);
    assert_eq!(read(&root.join(".gitignore")), "keep-me\n");
    assert!(!root.join(".cursor/rules/checkweave.mdc").exists());
}

#[test]
fn conflicting_mcp_entry_is_not_replaced() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::create_dir_all(root.join(".cursor")).unwrap();
    let original = "{\n  \"mcpServers\": {\n    \"checkweave\": {\n      \"command\": \"node\",\n      \"args\": [\"server.js\"],\n      \"env\": { \"TOKEN\": \"secret\" }\n    },\n    \"other\": { \"command\": \"keep\" }\n  }\n}\n";
    fs::write(root.join(".cursor/mcp.json"), original).unwrap();

    let error = Workspace::initialize(root, "cursor").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("conflicting"),
        "unexpected error: {message}"
    );
    assert_eq!(read(&root.join(".cursor/mcp.json")), original);
    assert!(!root.join(".cursor/rules/checkweave.mdc").exists());
}

#[test]
fn none_integration_does_not_touch_cursor_config() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::create_dir_all(root.join(".cursor")).unwrap();
    let original = b"{not-json";
    fs::write(root.join(".cursor/mcp.json"), original).unwrap();
    Workspace::initialize(root, "none").unwrap();
    assert_eq!(fs::read(root.join(".cursor/mcp.json")).unwrap(), original);
    assert!(!root.join(".cursor/rules").exists());
    assert!(root.join(".checkweave/workspace.json").is_file());
    assert!(read(&root.join(".gitignore")).contains(".checkweave/"));
}

#[test]
fn unsupported_integration_creates_nothing() {
    let dir = TempTree::new();
    let error = Workspace::initialize(dir.path(), "vscode").unwrap_err();
    assert!(format!("{error}").contains("unsupported integration"));
    assert!(!dir.path().join(".checkweave").exists());
}

#[test]
fn refuses_newer_workspace_version() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::create_dir_all(root.join(".checkweave")).unwrap();
    let marker = "{\n  \"version\": 99,\n  \"root\": \"placeholder\"\n}\n";
    fs::write(root.join(".checkweave/workspace.json"), marker).unwrap();
    let error = Workspace::initialize(root, "none").unwrap_err();
    assert!(format!("{error}").contains("unsupported workspace version"));
    assert_eq!(read(&root.join(".checkweave/workspace.json")), marker);
    assert!(!root.join(".gitignore").exists());
}

#[test]
fn linked_worktrees_have_distinct_roots() {
    let dir = TempTree::new();
    let main = dir.path().join("main");
    let linked = dir.path().join("linked");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-b", "main"]);
    git(&main, &["commit", "--allow-empty", "-m", "init"]);
    git(&main, &["worktree", "add", linked.to_str().unwrap()]);

    let main_ws = Workspace::discover(&main).unwrap();
    let linked_ws = Workspace::discover(&linked).unwrap();
    assert_eq!(main_ws.root, main.canonicalize().unwrap());
    assert_eq!(linked_ws.root, linked.canonicalize().unwrap());
    assert_ne!(main_ws.root, linked_ws.root);
    assert_ne!(main_ws.state_dir, linked_ws.state_dir);

    let sub = linked.join("src");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("note.txt"), "x").unwrap();
    assert_eq!(Workspace::discover(&sub).unwrap().root, linked_ws.root);

    Workspace::initialize(&main, "none").unwrap();
    Workspace::initialize(&linked, "none").unwrap();
    let main_marker: serde_json::Value =
        serde_json::from_str(&read(&main.join(".checkweave/workspace.json"))).unwrap();
    let linked_marker: serde_json::Value =
        serde_json::from_str(&read(&linked.join(".checkweave/workspace.json"))).unwrap();
    assert_eq!(
        main_marker["root"].as_str().unwrap(),
        main_ws.root.to_str().unwrap()
    );
    assert_eq!(
        linked_marker["root"].as_str().unwrap(),
        linked_ws.root.to_str().unwrap()
    );
}

#[test]
fn does_not_initialize_unrelated_ancestor() {
    let dir = TempTree::new();
    let parent = dir.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    let value = Workspace::initialize(&child, "none").unwrap();
    assert_eq!(
        PathBuf::from(value["root"].as_str().unwrap()),
        child.canonicalize().unwrap()
    );
    assert!(!parent.join(".checkweave").exists());
    assert!(child.join(".checkweave/workspace.json").is_file());

    let grand = child.join("grand");
    fs::create_dir(&grand).unwrap();
    Workspace::initialize(&grand, "none").unwrap();
    assert!(!parent.join(".checkweave").exists());
    assert!(child.join(".checkweave/workspace.json").is_file());
    assert!(!grand.join(".checkweave").exists());
}

#[test]
fn discovers_git_root_from_subdirectory() {
    let dir = TempTree::new();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    let nested = root.join("a/b");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("file.txt"), "hello").unwrap();
    let from_dir = Workspace::discover(&nested).unwrap();
    let from_file = Workspace::discover(&nested.join("file.txt")).unwrap();
    assert_eq!(from_dir.root, root.canonicalize().unwrap());
    assert_eq!(from_file.root, from_dir.root);
    assert_eq!(from_dir.state_dir, from_dir.root.join(".checkweave"));
}

#[test]
fn discovers_nearest_initialized_ancestor_without_git() {
    let dir = TempTree::new();
    let parent = dir.path().join("parent");
    fs::create_dir_all(parent.join("sub/leaf")).unwrap();
    Workspace::initialize(&parent, "none").unwrap();
    let found = Workspace::discover(&parent.join("sub/leaf")).unwrap();
    assert_eq!(found.root, parent.canonicalize().unwrap());
}

#[test]
fn nested_git_repo_stays_distinct_from_parent_workspace() {
    let dir = TempTree::new();
    let parent = dir.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    Workspace::initialize(&parent, "none").unwrap();
    let parent_marker = fs::read(parent.join(".checkweave/workspace.json")).unwrap();
    git(&child, &["init", "-b", "main"]);
    let found = Workspace::discover(&child).unwrap();
    assert_eq!(found.root, child.canonicalize().unwrap());
    Workspace::initialize(&child, "none").unwrap();
    assert_eq!(
        fs::read(parent.join(".checkweave/workspace.json")).unwrap(),
        parent_marker
    );
    assert!(child.join(".checkweave/workspace.json").is_file());
    assert_ne!(found.state_dir, parent.join(".checkweave"));
}

#[test]
fn concurrent_initialization_preserves_other_servers() {
    let dir = TempTree::new();
    let root = dir.path();
    fs::create_dir_all(root.join(".cursor")).unwrap();
    fs::write(
        root.join(".cursor/mcp.json"),
        "{\"mcpServers\":{\"other\":{\"command\":\"keep\",\"args\":[\"a\"]}}}\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "# comment\n").unwrap();
    let path = root.to_path_buf();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let path = path.clone();
            scope.spawn(move || {
                Workspace::initialize(&path, "cursor").unwrap();
            });
        }
    });
    let gitignore = read(&root.join(".gitignore"));
    assert_eq!(gitignore.matches(".checkweave/").count(), 1);
    assert!(gitignore.contains("# comment\n"));
    let mcp: serde_json::Value =
        serde_json::from_str(&read(&root.join(".cursor/mcp.json"))).unwrap();
    assert_eq!(mcp["mcpServers"]["other"]["command"], "keep");
    assert!(mcp["mcpServers"]["checkweave"]["command"].is_string());
    assert_eq!(mcp["mcpServers"]["checkweave"]["args"][2], "mcp");
}
