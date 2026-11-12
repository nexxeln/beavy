//! Snapshots and the trash end to end: the beavy binary against a real store.
//! Nothing here mounts, so these run on any Linux machine.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
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
            .env_remove("SUDO_UID")
            .env_remove("SUDO_GID")
            .env_remove("SUDO_USER")
            .stdin(Stdio::null());
        command
    }

    /// beavy as root through sudo, run by `uid`.
    fn sudo(&self, args: &[&str], (uid, gid): (u32, u32)) -> Command {
        let mut command = self.beavy(args);
        command
            .env("SUDO_UID", uid.to_string())
            .env("SUDO_GID", gid.to_string());
        command
    }

    /// Hand everything in the fixture to `uid`, as if that user had made it.
    fn give_to(&self, (uid, gid): (u32, u32)) {
        let root = self.home.parent().unwrap();
        let status = Command::new("chown")
            .arg("-R")
            .arg(format!("{uid}:{gid}"))
            .arg(root)
            .status()
            .unwrap();
        assert!(status.success());
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

/// When the tests run as root, the user that stands in for the one who ran sudo.
fn sudo_user() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions.
    (unsafe { libc::geteuid() } == 0).then_some((65534, 65534))
}

/// Every path under `root`, `root` included.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    let mut next = 0;
    while next < paths.len() {
        let path = paths[next].clone();
        next += 1;
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                paths.push(entry.unwrap().path());
            }
        }
    }
    paths
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
fn a_store_is_made_only_in_an_empty_directory() {
    let f = Fixture::new();
    let data = f.home.join("data");
    fs::create_dir_all(data.join("tmp/build-cache")).unwrap();
    fs::create_dir_all(data.join("layers/photos")).unwrap();
    let refused = f.output(&[
        "snapshots",
        "create",
        "--store",
        data.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(error(&refused)["reason"], "invalid_arguments");
    assert!(data.join("tmp/build-cache").is_dir());
    assert!(data.join("layers/photos").is_dir());
    assert!(!data.join("store.json").exists());
}

#[test]
fn record_ids_never_become_paths_outside_the_store() {
    let f = Fixture::new();
    let victim = f.home.join("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("data"), "keep").unwrap();

    let id = f.ok(&["snapshots", "create"]).trim().to_string();
    let record = f.store.join("snapshots").join(format!("{id}.json"));
    let text = fs::read_to_string(&record).unwrap();
    let layer = &f.entries("layers")[0];
    fs::write(&record, text.replace(layer.as_str(), "../../home/victim")).unwrap();
    let refused = f.output(&["snapshots", "list", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(error(&refused)["reason"], "invalid_store");
    fs::write(&record, text).unwrap();

    f.ok(&["snapshots", "delete", &id]);
    let entry = f.store.join("trash").join(&id).join("entry.json");
    let text = fs::read_to_string(&entry).unwrap();
    fs::write(&entry, text.replace(&id, "../../home/victim")).unwrap();
    let refused = f.output(&["trash", "empty", "--yes", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(error(&refused)["reason"], "invalid_store");
    assert_eq!(fs::read_to_string(victim.join("data")).unwrap(), "keep");
}

#[test]
fn a_symlinked_head_is_never_written_through() {
    let f = Fixture::new();
    f.git(&["init", "-q", "-b", "main"]);
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "first"]);
    let other = f.home.join("other.git");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let head = f.project.join(".git/HEAD");
    fs::remove_file(&head).unwrap();
    std::os::unix::fs::symlink(other.join("HEAD"), &head).unwrap();

    f.ok(&["snapshots", "create"]);
    assert_eq!(
        fs::read_to_string(other.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
}

#[test]
fn emptying_the_trash_deletes_only_what_was_approved() {
    use std::os::fd::{FromRawFd, OwnedFd};

    let f = Fixture::new();
    let first = f.ok(&["snapshots", "create"]).trim().to_string();
    let second = f.ok(&["snapshots", "create"]).trim().to_string();
    f.ok(&["snapshots", "delete", &first]);

    let (mut controller, mut terminal) = (0, 0);
    // SAFETY: both out pointers are valid; null name, termios, and winsize are allowed.
    let rc = unsafe {
        libc::openpty(
            &mut controller,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: openpty returned two open descriptors that nothing else owns.
    let (controller, terminal) = unsafe {
        (
            OwnedFd::from_raw_fd(controller),
            OwnedFd::from_raw_fd(terminal),
        )
    };
    let child = Command::new(env!("CARGO_BIN_EXE_beavy"))
        .args(["trash", "empty"])
        .current_dir(&f.project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &f.home)
        .env("BEAVY_STORE", &f.store)
        .env("TERM", "xterm")
        .stdin(Stdio::from(terminal.try_clone().unwrap()))
        .stderr(Stdio::from(terminal))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut screen = fs::File::from(controller);
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("Empty the trash?") {
        match screen.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    let prompt = String::from_utf8_lossy(&seen).into_owned();
    assert!(prompt.contains("Deletes 1 snapshot"), "{prompt}");

    f.ok(&["snapshots", "delete", &second]);
    screen.write_all(b"y\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let left = f.json(&["trash", "list"])["entries"].clone();
    assert_eq!(left.as_array().unwrap().len(), 1, "{left}");
    assert_eq!(left[0]["id"], second.as_str());
}

#[test]
fn through_sudo_the_store_belongs_to_the_user_who_ran_it() {
    let Some(user) = sudo_user() else {
        return;
    };
    let f = Fixture::new();
    f.give_to(user);
    let created = f
        .sudo(&["snapshots", "create"], user)
        .env_remove("BEAVY_STORE")
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let local = f.home.join(".local");
    let store = walk(&local);
    assert!(store.len() > 8, "{store:?}");
    for path in store {
        let owner = fs::symlink_metadata(&path).unwrap().uid();
        assert_eq!(owner, user.0, "{}", path.display());
    }

    let refused = f.output(&["snapshots", "create", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(error(&refused)["reason"], "permission_denied");

    let dir = tempfile::tempdir().unwrap();
    let rooted = dir.path().join("store");
    let rooted = rooted.to_str().unwrap();
    assert!(
        f.output(&["snapshots", "create", "--store", rooted])
            .status
            .success()
    );
    let refused = f
        .sudo(&["snapshots", "list", "--store", rooted, "--json"], user)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(error(&refused)["reason"], "permission_denied");
}

#[test]
fn through_sudo_beavy_never_follows_a_link_in_the_store() {
    let Some(user) = sudo_user() else {
        return;
    };
    let f = Fixture::new();
    f.give_to(user);
    let created = f.sudo(&["snapshots", "create"], user).output().unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let guarded = tempfile::tempdir().unwrap();
    let proof = guarded.path().join("proof");
    fs::remove_file(f.store.join("lock")).unwrap();
    std::os::unix::fs::symlink(&proof, f.store.join("lock")).unwrap();
    let refused = f.sudo(&["snapshots", "create"], user).output().unwrap();
    assert!(!refused.status.success());
    assert!(!proof.exists());

    fs::remove_file(f.store.join("lock")).unwrap();
    let created = f.output(&["snapshots", "create"]);
    assert!(created.status.success());
    for path in walk(&f.store) {
        let owner = fs::symlink_metadata(&path).unwrap().uid();
        assert_eq!(owner, user.0, "{}", path.display());
    }
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
