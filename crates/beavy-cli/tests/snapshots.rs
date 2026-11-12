//! Snapshots and the trash end to end: the beavy binary against a real store.
//! Nothing here mounts, so these run on any Linux machine.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fixture = Fixture {
            home: root.join("home"),
            project: root.join("acme"),
            store: root.join("store"),
            _dir: dir,
        };
        fs::create_dir_all(fixture.project.join("src")).unwrap();
        fs::create_dir_all(&fixture.home).unwrap();
        fs::write(fixture.project.join("README.md"), "readme\n").unwrap();
        fs::write(fixture.project.join("src/main.txt"), "main\n").unwrap();
        fixture
    }

    fn beavy(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_beavy"));
        command
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("BEAVY_STORE", &self.store)
            .env_remove("XDG_DATA_HOME")
            .env_remove("__BEAVY_CRASH_AT")
            .stdin(Stdio::null());
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.beavy(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "beavy {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        serde_json::from_str(&self.ok(&args)).unwrap()
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(&self.project)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {args:?}");
    }

    fn entries(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.store.join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn error(output: &Output) -> Value {
    assert!(output.stdout.is_empty());
    serde_json::from_slice(&output.stderr).expect("one JSON error")
}

#[test]
fn create_prints_the_id_and_copies_the_project_into_the_store() {
    let f = Fixture::new();
    let created = f.output(&["snapshots", "create", "--name", "base"]);
    assert!(created.status.success());
    let id = String::from_utf8(created.stdout).unwrap();
    assert!(
        id.starts_with("snap_") && id.ends_with('\n') && id.trim().len() == 15,
        "{id:?}"
    );
    let stderr = String::from_utf8(created.stderr).unwrap();
    assert!(
        stderr.contains(&format!("Created {}", id.trim())),
        "{stderr}"
    );

    let inspected = f.json(&["snapshots", "inspect", "base"]);
    let snapshot = &inspected["snapshot"];
    assert_eq!(snapshot["id"], id.trim());
    assert_eq!(snapshot["name"], "base");
    assert_eq!(snapshot["parent"], Value::Null);
    assert_eq!(snapshot["source"], f.project.to_str().unwrap());
    assert_eq!(snapshot["depth"], 1);
    assert_eq!(snapshot["children"], serde_json::json!([]));
    assert_eq!(inspected["store"]["path"], f.store.to_str().unwrap());

    let layers = f.entries("layers");
    assert_eq!(layers.len(), 1);
    let layer = f.store.join("layers").join(&layers[0]);
    assert_eq!(
        fs::read_to_string(layer.join("src/main.txt")).unwrap(),
        "main\n"
    );
    assert!(!f.project.join(".beavy").exists());

    let list = f.json(&["snapshots", "list"]);
    assert_eq!(list["snapshots"].as_array().unwrap().len(), 1);
    let table = f.ok(&["snapshots", "list"]);
    assert!(
        table.starts_with("ID\tName\tParent\tDepth\tCreated\n"),
        "{table}"
    );

    let taken = f.output(&["snapshots", "create", "--name", "base", "--json"]);
    assert_eq!(taken.status.code(), Some(1));
    assert_eq!(error(&taken)["reason"], "name_taken");
    let invalid = f.output(&["snapshots", "create", "--name", "Base", "--json"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(error(&invalid)["reason"], "invalid_arguments");
    let missing = f.output(&["snapshots", "inspect", "nope", "--json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(error(&missing)["reason"], "not_found");
}

#[test]
fn the_default_store_is_per_project_and_outside_it() {
    let f = Fixture::new();
    let output = f
        .beavy(&["snapshots", "create", "--json"])
        .env_remove("BEAVY_STORE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let store = PathBuf::from(result["store"]["path"].as_str().unwrap());
    assert_eq!(
        store.parent().unwrap(),
        f.home.join(".local/share/beavy/stores")
    );
    assert!(
        store
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("acme-")
    );

    let nested = f
        .beavy(&["snapshots", "list", "--json"])
        .env_remove("BEAVY_STORE")
        .current_dir(f.project.join("src"))
        .output()
        .unwrap();
    let listed: Value = serde_json::from_slice(&nested.stdout).unwrap();
    assert_eq!(listed["store"]["path"], store.to_str().unwrap());
    assert_eq!(listed["snapshots"].as_array().unwrap().len(), 1);
}

#[test]
fn git_state_is_kept_with_a_detached_head_and_unsafe_states_are_refused() {
    let f = Fixture::new();
    f.git(&["init", "-q"]);
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "init"]);
    fs::write(f.project.join("README.md"), "edited\n").unwrap();
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&f.project)
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap().trim().to_string();

    let snapshot = f.json(&["snapshots", "create"])["snapshot"].clone();
    assert_eq!(snapshot["commit"], head.as_str());
    let layer = f.store.join("layers").join(&f.entries("layers")[0]);
    assert_eq!(
        fs::read_to_string(layer.join(".git/HEAD")).unwrap().trim(),
        head
    );
    assert_eq!(
        fs::read_to_string(layer.join("README.md")).unwrap(),
        "edited\n"
    );
    assert!(
        fs::read_to_string(f.project.join(".git/HEAD"))
            .unwrap()
            .starts_with("ref: ")
    );

    fs::write(f.project.join(".git/MERGE_HEAD"), &head).unwrap();
    let refused = f.output(&["snapshots", "create", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(error(&refused)["reason"], "unsupported_git_state");
    fs::remove_file(f.project.join(".git/MERGE_HEAD")).unwrap();

    let linked = f.project.parent().unwrap().join("linked");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        linked.to_str().unwrap(),
    ]);
    let refused = f
        .beavy(&["snapshots", "create", "--json"])
        .current_dir(&linked)
        .output()
        .unwrap();
    let refused = error(&refused);
    assert_eq!(refused["reason"], "unsupported_git_state");
    assert_eq!(refused["next"][0]["argv"][4], f.project.to_str().unwrap());
}

#[test]
fn deleted_snapshots_wait_in_the_trash_until_it_is_emptied() {
    let f = Fixture::new();
    let id = f
        .ok(&["snapshots", "create", "--name", "base"])
        .trim()
        .to_string();
    assert_eq!(
        f.json(&["snapshots", "delete", "base"])["outcome"],
        "deleted"
    );
    assert!(
        f.json(&["snapshots", "list"])["snapshots"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let entries = f.json(&["trash", "list"])["entries"].clone();
    assert_eq!(entries[0]["id"], id.as_str());
    assert_eq!(entries[0]["kind"], "snapshot");
    assert_eq!(f.entries("layers").len(), 1);

    assert_eq!(f.json(&["trash", "restore", &id])["outcome"], "restored");
    assert_eq!(
        f.json(&["snapshots", "inspect", "base"])["snapshot"]["id"],
        id.as_str()
    );
    f.ok(&["snapshots", "delete", &id]);

    let refused = f.output(&["trash", "empty", "--json"]);
    assert_eq!(refused.status.code(), Some(2));
    let refused = error(&refused);
    assert_eq!(refused["reason"], "confirmation_required");
    assert_eq!(refused["status"], "action_required");
    assert_eq!(refused["next"][0]["argv"][3], "--yes");
    assert_eq!(f.entries("layers").len(), 1);

    let emptied = f.json(&["trash", "empty", "--yes"]);
    assert_eq!(emptied["outcome"], "emptied");
    assert_eq!(emptied["entries"][0]["id"], id.as_str());
    assert!(f.entries("layers").is_empty());
    assert!(f.entries("trash").is_empty());
    assert_eq!(f.json(&["trash", "empty", "--yes"])["outcome"], "unchanged");
}

#[test]
fn interrupted_imports_and_deletes_are_repaired() {
    let f = Fixture::new();
    let killed = f
        .beavy(&["snapshots", "create"])
        .env("__BEAVY_CRASH_AT", "snapshot-import")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    assert_eq!(f.entries("tmp").len(), 1);
    assert!(
        f.json(&["snapshots", "list"])["snapshots"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let id = f.ok(&["snapshots", "create"]).trim().to_string();
    assert!(f.entries("tmp").is_empty());
    assert_eq!(f.entries("layers").len(), 1);

    let killed = f
        .beavy(&["snapshots", "delete", &id])
        .env("__BEAVY_CRASH_AT", "trash-move")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    f.ok(&["snapshots", "create", "--name", "second"]);
    assert!(f.entries("trash").is_empty());
    assert_eq!(
        f.json(&["snapshots", "inspect", &id])["snapshot"]["id"],
        id.as_str()
    );
}

#[test]
fn a_store_inside_the_project_is_left_out_of_the_import() {
    let f = Fixture::new();
    let inner = f.project.join(".beavy-store");
    let created = f
        .beavy(&["snapshots", "create", "--store", inner.to_str().unwrap()])
        .env_remove("BEAVY_STORE")
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let layers: Vec<PathBuf> = fs::read_dir(inner.join("layers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(layers.len(), 1);
    assert!(!layers[0].join(".beavy-store").exists());
    assert!(Path::new(&layers[0]).join("README.md").exists());
}
