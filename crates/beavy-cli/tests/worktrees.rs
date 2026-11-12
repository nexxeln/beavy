//! Worktrees end to end: the beavy binary against a real store and real overlay
//! mounts, as root or in a user namespace, whichever this process has.
//!
//! On a machine that can't mount worktrees these tests skip, unless
//! BEAVY_REQUIRE_WORKTREES is set, as CI sets it. BEAVY_EXPECT_PRIVILEGE (`root`
//! or `userns`) makes them fail when beavy mounts the other way.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    store: PathBuf,
}

impl Fixture {
    /// A project with a base snapshot, or None when worktrees can't run here.
    #[allow(clippy::print_stderr, reason = "says why a test skipped")]
    fn new() -> Option<Fixture> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fixture = Fixture {
            home: root.join("home"),
            project: root.join("project"),
            store: root.join("store"),
            _dir: dir,
        };
        let p = &fixture.project;
        fs::create_dir_all(p.join("src")).unwrap();
        fs::create_dir_all(p.join("docs/old")).unwrap();
        fs::create_dir_all(&fixture.home).unwrap();
        fs::write(p.join("README.md"), "readme\n").unwrap();
        fs::write(p.join("src/main.txt"), "main\n").unwrap();
        fs::write(p.join("src/lib.txt"), "lib\n").unwrap();
        fs::write(p.join("docs/old/page.txt"), "page\n").unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(p)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        let _ = git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-qm", "init"]);
        fixture.ok(&["snapshots", "create", "--name", "base"]);

        let probe = fixture
            .beavy(&["worktrees", "create", "base", "--json"])
            .output()
            .unwrap();
        if !probe.status.success() {
            let stderr = String::from_utf8_lossy(&probe.stderr);
            assert!(
                stderr.contains("backend_unavailable")
                    && std::env::var_os("BEAVY_REQUIRE_WORKTREES").is_none(),
                "worktrees are required here but can't run: {stderr}"
            );
            eprintln!("skipping: this machine can't mount worktrees: {stderr}");
            return None;
        }
        let created = document(&probe.stdout);
        if let Some(expected) = std::env::var_os("BEAVY_EXPECT_PRIVILEGE") {
            assert_eq!(created["backend"]["privilege"], expected.to_str().unwrap());
        }
        let id = created["worktree"]["id"].as_str().unwrap().to_string();
        fixture.ok(&["worktrees", "delete", &id]);
        fixture.ok(&["trash", "empty", "--yes"]);
        Some(fixture)
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

    /// Run beavy, assert it succeeded, and return its stdout without the newline.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.beavy(args).output().unwrap();
        assert!(
            output.status.success(),
            "beavy {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_string()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        document(self.ok(&args).as_bytes())
    }

    fn create(&self, snapshot: &str) -> String {
        self.ok(&["worktrees", "create", snapshot])
    }

    /// `sh -c script` in a worktree.
    fn run(&self, worktree: &str, script: &str) -> Output {
        self.beavy(&["worktrees", "run", worktree, "--", "sh", "-c", script])
            .output()
            .unwrap()
    }

    /// `sh -c script` in a worktree, asserting success; its stdout.
    fn sh(&self, worktree: &str, script: &str) -> String {
        let output = self.run(worktree, script);
        assert!(
            output.status.success(),
            "{script:?} failed in {worktree}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// beavy as root through sudo, run by `user`.
    fn sudo(&self, args: &[&str], (uid, gid): (u32, u32)) -> Command {
        let mut command = self.beavy(args);
        command
            .env("SUDO_UID", uid.to_string())
            .env("SUDO_GID", gid.to_string());
        command
    }

    /// beavy as `user`, with no privilege. Run `give_to` first.
    fn as_user(&self, args: &[&str], (uid, gid): (u32, u32)) -> Command {
        let mut command = Command::new("setpriv");
        command
            .args([
                &format!("--reuid={uid}"),
                &format!("--regid={gid}"),
                "--clear-groups",
            ])
            .arg(self.home.with_file_name("beavy"))
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

    /// Hand everything in the fixture, and a copy of beavy, to `user`.
    fn give_to(&self, (uid, gid): (u32, u32)) {
        fs::copy(
            env!("CARGO_BIN_EXE_beavy"),
            self.home.with_file_name("beavy"),
        )
        .unwrap();
        let status = Command::new("chown")
            .arg("-R")
            .arg(format!("{uid}:{gid}"))
            .arg(self.home.parent().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
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

fn document(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("one JSON document")
}

/// Run `command`, assert it succeeded, and return its stdout without the newline.
fn succeed(mut command: Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

/// Run `command`, which passes --json, assert it failed, and return its error
/// document.
fn refuse(mut command: Command) -> Value {
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stdout.is_empty());
    document(&output.stderr)
}

/// The user a root test acts as through sudo, or None when not root.
fn sudo_user() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions.
    (unsafe { libc::geteuid() } == 0).then_some((65534, 65534))
}

/// Kill a process and wait until it no longer holds any mount.
fn stop(pid: &str) {
    Command::new("kill").args(["-KILL", pid]).status().unwrap();
    let start = Instant::now();
    while fs::read_to_string(format!("/proc/{pid}/mountinfo")).is_ok_and(|table| !table.is_empty())
    {
        assert!(start.elapsed() < Duration::from_secs(10), "{pid} lives on");
        std::thread::sleep(Duration::from_millis(20));
    }
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

fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            found.push(path.strip_prefix(dir).unwrap().display().to_string());
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn worktrees_see_the_snapshot_and_keep_their_writes_to_themselves() {
    let Some(f) = Fixture::new() else { return };
    let before = listing(&f.project);
    let (a, b) = (f.create("base"), f.create("base"));

    assert_eq!(f.sh(&a, "cat README.md src/main.txt"), "readme\nmain\n");
    f.sh(
        &a,
        "echo changed > README.md && echo new > new.txt && rm src/lib.txt && rm -r docs",
    );
    assert_eq!(
        f.sh(&a, "cat README.md new.txt; ls src; ls -d docs 2>&1 || true")
            .lines()
            .count(),
        4
    );
    assert_eq!(f.sh(&a, "ls src"), "main.txt\n");

    assert_eq!(
        f.sh(&b, "cat README.md; ls src; cat docs/old/page.txt"),
        "readme\nlib.txt\nmain.txt\npage\n"
    );
    assert!(f.run(&b, "test -e new.txt").status.code() == Some(1));
    assert_eq!(listing(&f.project), before);
    assert_eq!(
        fs::read_to_string(f.project.join("README.md")).unwrap(),
        "readme\n"
    );

    let fresh = f.create("base");
    assert_eq!(
        f.sh(&fresh, "cat README.md; ls src"),
        "readme\nlib.txt\nmain.txt\n"
    );
}

#[test]
fn run_passes_through_the_user_exit_status_signals_streams_and_directory() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    assert_eq!(f.sh(&wt, "id -u").trim(), uid.to_string());
    assert_eq!(f.sh(&wt, "echo $BEAVY_WORKTREE").trim(), wt);
    assert_eq!(f.sh(&wt, "pwd").trim(), f.project.display().to_string());

    let status = f.run(&wt, "echo out; echo err >&2; exit 7");
    assert_eq!(status.status.code(), Some(7));
    assert_eq!(status.stdout, b"out\n");
    assert_eq!(status.stderr, b"err\n");
    assert_eq!(
        f.run(&wt, "kill -TERM $$").status.signal(),
        Some(libc::SIGTERM)
    );

    let mut child = f
        .beavy(&["worktrees", "run", &wt, "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"ping").unwrap();
    assert_eq!(child.wait_with_output().unwrap().stdout, b"ping");

    let nested = f
        .beavy(&["worktrees", "run", &wt, "--", "pwd"])
        .current_dir(f.project.join("src"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(nested.stdout).unwrap().trim(),
        f.project.join("src").display().to_string()
    );

    let missing = f
        .beavy(&["worktrees", "run", &wt, "--json", "--", "no-such-command"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(document(&missing.stderr)["reason"], "not_found");
}

#[test]
fn a_sealed_worktree_becomes_a_snapshot_with_its_deletes() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");
    f.sh(&wt, "echo sealed > new.txt && rm src/lib.txt && rm -r docs && mkdir docs && echo fresh > docs/only.txt");
    let sealed = f.json(&["snapshots", "create", "--worktree", &wt, "--name", "sealed"]);
    assert_eq!(
        sealed["snapshot"]["parent"],
        f.json(&["snapshots", "inspect", "base"])["snapshot"]["id"]
    );
    assert_eq!(sealed["snapshot"]["depth"], 2);
    assert_eq!(sealed["worktree"]["snapshot"], sealed["snapshot"]["id"]);

    let next = f.create("sealed");
    let seen = "cat new.txt; ls src; ls docs; test -e docs/old && echo leaked; true";
    assert_eq!(f.sh(&next, seen), "sealed\nmain.txt\nonly.txt\n");
    assert_eq!(f.sh(&wt, seen), "sealed\nmain.txt\nonly.txt\n");
    assert_eq!(f.sh(&f.create("base"), "ls src"), "lib.txt\nmain.txt\n");

    let refused = f
        .beavy(&["snapshots", "delete", "base", "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(document(&refused.stderr)["reason"], "snapshot_in_use");
}

#[test]
fn long_chains_are_squashed_without_changing_what_worktrees_see() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");
    let mut depths = Vec::new();
    for i in 1..=10 {
        let script = format!(
            "echo {i} >> log && echo {i} > f{i} && rm -f f{} && mkdir -p made && echo {i} > made/m{i} && rm -f made/m{} && if [ {i} = 4 ]; then rm -r src && mkdir src && echo n > src/n; fi",
            i - 2,
            i - 2
        );
        f.sh(&wt, &script);
        let sealed = f.json(&["snapshots", "create", "--worktree", &wt]);
        depths.push(sealed["snapshot"]["depth"].as_u64().unwrap());
    }
    assert_eq!(depths, [2, 3, 4, 5, 6, 7, 8, 2, 3, 4]);
    let expected = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\nf10\nf9\nn\nm10\nm9\n";
    let script = "cat log; ls -d f*; ls src; ls made";
    assert_eq!(f.sh(&wt, script), expected);
    let latest = f.json(&["worktrees", "inspect", &wt])["worktree"]["snapshot"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(f.sh(&f.create(&latest), script), expected);
}

#[test]
fn deleting_a_worktree_and_emptying_the_trash_leaves_nothing() {
    let Some(f) = Fixture::new() else { return };
    let layers = f.entries("layers");
    let wt = f.create("base");
    f.sh(&wt, "mkdir -p a/b && echo x > a/b/c && rm README.md");
    let deleted = f.json(&["worktrees", "delete", &wt]);
    assert_eq!(deleted["outcome"], "deleted");
    assert!(f.entries("worktrees").is_empty());
    assert_eq!(f.json(&["trash", "list"])["entries"][0]["kind"], "worktree");

    f.ok(&["trash", "restore", &wt]);
    assert_eq!(f.sh(&wt, "cat a/b/c"), "x\n");
    f.ok(&["worktrees", "delete", &wt]);
    f.ok(&["trash", "empty", "--yes"]);
    assert!(f.entries("worktrees").is_empty());
    assert!(f.entries("trash").is_empty());
    assert!(f.entries("tmp").is_empty());
    assert_eq!(f.entries("layers"), layers);
}

#[test]
fn interrupted_creates_and_seals_are_repaired() {
    let Some(f) = Fixture::new() else { return };
    let killed = f
        .beavy(&["worktrees", "create", "base"])
        .env("__BEAVY_CRASH_AT", "worktree-create")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    assert_eq!(f.entries("tmp").len(), 1);
    assert!(
        f.json(&["worktrees", "list"])["worktrees"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let wt = f.create("base");
    assert!(f.entries("tmp").is_empty());
    assert_eq!(f.entries("worktrees"), std::slice::from_ref(&wt));

    f.sh(&wt, "echo kept > kept.txt");
    let killed = f
        .beavy(&["snapshots", "create", "--worktree", &wt])
        .env("__BEAVY_CRASH_AT", "seal-journal")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    assert_eq!(f.sh(&wt, "cat kept.txt"), "kept\n");
    assert_eq!(
        f.json(&["snapshots", "list"])["snapshots"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let killed = f
        .beavy(&["snapshots", "create", "--worktree", &wt])
        .env("__BEAVY_CRASH_AT", "seal-layer")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    assert_eq!(f.sh(&wt, "cat kept.txt"), "kept\n");
    let snapshots = f.json(&["snapshots", "list"])["snapshots"].clone();
    let snapshots = snapshots.as_array().unwrap();
    assert_eq!(snapshots.len(), 2);
    let sealed = snapshots
        .iter()
        .find(|snapshot| snapshot["method"] == "seal")
        .expect("the seal finished");
    assert_eq!(
        f.json(&["worktrees", "inspect", &wt])["worktree"]["snapshot"],
        sealed["id"]
    );
    assert_eq!(
        f.sh(&f.create(sealed["id"].as_str().unwrap()), "cat kept.txt"),
        "kept\n"
    );
    assert!(f.entries("tmp").is_empty());
}

#[test]
fn commands_in_one_worktree_share_a_mount_and_block_changes() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");
    let mut first = f
        .beavy(&[
            "worktrees",
            "run",
            &wt,
            "--",
            "sh",
            "-c",
            "readlink /proc/self/ns/mnt > marker && sleep 3",
        ])
        .spawn()
        .unwrap();
    let start = Instant::now();
    while f.json(&["worktrees", "inspect", &wt])["worktree"]["runs"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the first run never registered"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        f.sh(&wt, "readlink /proc/self/ns/mnt; cat marker")
            .lines()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "the second run has its own mount"
    );

    let refused = f
        .beavy(&["snapshots", "create", "--worktree", &wt, "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(document(&refused.stderr)["reason"], "worktree_in_use");
    let refused = f
        .beavy(&["worktrees", "delete", &wt, "--json"])
        .output()
        .unwrap();
    assert_eq!(document(&refused.stderr)["reason"], "worktree_in_use");

    assert!(first.wait().unwrap().success());
    f.ok(&["snapshots", "create", "--worktree", &wt]);
    assert_eq!(f.sh(&wt, "cat marker").lines().count(), 1);
}

#[test]
fn concurrent_creates_never_clobber_each_other() {
    let Some(f) = Fixture::new() else { return };
    let children: Vec<_> = (0..6)
        .map(|i| {
            f.beavy(&["worktrees", "create", "base", "--name", &format!("w{i}")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ids: Vec<String> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        })
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 6);
    assert_eq!(f.entries("worktrees"), ids);
    for (i, id) in ids.iter().enumerate() {
        f.sh(id, &format!("echo {i} > mine"));
    }
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(f.sh(id, "cat mine"), format!("{i}\n"));
    }
}

#[test]
fn root_mounts_a_worktree_for_every_process() {
    let Some(f) = Fixture::new() else { return };
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let wt = f.create("base");
        let refused = f
            .beavy(&["worktrees", "mount", &wt, "/tmp", "--json"])
            .output()
            .unwrap();
        assert_eq!(document(&refused.stderr)["reason"], "permission_denied");
        return;
    }
    let wt = f.create("base");
    let above = f.store.parent().unwrap().to_str().unwrap();
    let error = refuse(f.beavy(&["worktrees", "mount", &wt, above, "--json"]));
    assert_eq!(error["reason"], "invalid_arguments");
    assert!(
        f.json(&["worktrees", "inspect", &wt])["worktree"]["mounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let dir = f.home.join("mnt");
    fs::create_dir(&dir).unwrap();
    let mounted = f.json(&["worktrees", "mount", &wt, dir.to_str().unwrap()]);
    assert_eq!(mounted["outcome"], "mounted");
    fs::write(dir.join("from-host"), "host\n").unwrap();
    assert_eq!(f.sh(&wt, "cat from-host"), "host\n");
    f.sh(&wt, "echo inside > from-run");
    assert_eq!(
        fs::read_to_string(dir.join("from-run")).unwrap(),
        "inside\n"
    );

    f.ok(&["snapshots", "create", "--worktree", &wt]);
    assert_eq!(
        fs::read_to_string(dir.join("from-run")).unwrap(),
        "inside\n"
    );
    assert_eq!(
        f.json(&["worktrees", "unmount", &wt])["outcome"],
        "unmounted"
    );
    assert!(!dir.join("from-run").exists());
    f.ok(&["worktrees", "mount", &wt, dir.to_str().unwrap()]);
    f.ok(&["worktrees", "delete", &wt]);
    assert!(!dir.join("from-run").exists());
}

#[test]
fn background_jobs_never_block_later_runs_or_outlive_a_seal() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");

    let job = f.sh(&wt, "sleep 60 >/dev/null 2>&1 & echo $!");
    let job = job.trim();
    let namespaces = f.sh(
        &wt,
        &format!("readlink /proc/{job}/ns/mnt; readlink /proc/self/ns/mnt"),
    );
    let namespaces: Vec<&str> = namespaces.lines().collect();
    assert_eq!(namespaces.len(), 2);
    assert_eq!(namespaces[0], namespaces[1], "a later run joins the job");
    stop(job);

    let job = f.sh(
        &wt,
        "python3 -c 'import subprocess; print(subprocess.Popen([\"sleep\", \"60\"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).pid)'",
    );
    let job = job.trim();
    let runs = f.json(&["worktrees", "inspect", &wt])["worktree"]["runs"].clone();
    assert!(
        runs.as_array()
            .unwrap()
            .iter()
            .any(|run| run.as_u64().is_some_and(|pid| pid.to_string() == job)),
        "{runs}"
    );
    for args in [
        &["snapshots", "create", "--worktree", &wt, "--json"][..],
        &["worktrees", "delete", &wt, "--json"],
    ] {
        let error = refuse(f.beavy(args));
        assert_eq!(error["reason"], "worktree_in_use");
        assert!(error["message"].as_str().unwrap().contains(job), "{error}");
    }
    stop(job);
    f.ok(&["snapshots", "create", "--worktree", &wt]);
}

#[test]
fn a_directory_the_worktree_lacks_starts_the_command_at_the_project() {
    let Some(f) = Fixture::new() else { return };
    let wt = f.create("base");
    f.sh(&wt, "rm -r src");
    let mut command = f.beavy(&["worktrees", "run", &wt, "--", "pwd"]);
    command.current_dir(f.project.join("src"));
    assert_eq!(succeed(command), f.project.display().to_string());
}

#[test]
fn worktree_records_are_checked_before_their_ids_become_paths() {
    let Some(f) = Fixture::new() else { return };
    let victim = f.home.join("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("data"), "keep").unwrap();
    let wt = f.ok(&["worktrees", "create", "base", "--name", "evil"]);
    let dir = f.store.join("worktrees").join(&wt);
    let record = dir.join("worktree.json");
    let text = fs::read_to_string(&record).unwrap();
    let snapshot = f.json(&["snapshots", "inspect", "base"])["snapshot"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for replaced in [&wt, &snapshot] {
        fs::write(
            &record,
            text.replace(replaced.as_str(), "../../home/victim"),
        )
        .unwrap();
        for args in [
            &["worktrees", "list", "--json"][..],
            &["worktrees", "delete", "evil", "--json"],
            &["worktrees", "run", "evil", "--json", "--", "true"],
        ] {
            assert_eq!(refuse(f.beavy(args))["reason"], "invalid_store", "{args:?}");
        }
    }
    fs::write(&record, &text).unwrap();

    f.sh(&wt, "echo sealed > sealed.txt");
    let killed = f
        .beavy(&["snapshots", "create", "--worktree", &wt])
        .env("__BEAVY_CRASH_AT", "seal-journal")
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(libc::SIGKILL));
    let journal = dir.join("seal.json");
    let text = fs::read_to_string(&journal).unwrap();
    let layer = serde_json::from_str::<Value>(&text).unwrap()["layer"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(&journal, text.replace(&layer, "../../home/victim")).unwrap();
    assert_eq!(
        refuse(f.beavy(&["worktrees", "run", &wt, "--json", "--", "true"]))["reason"],
        "invalid_store"
    );
    let mut left: Vec<_> = fs::read_dir(&victim)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["data"]);
    assert_eq!(fs::read_to_string(victim.join("data")).unwrap(), "keep");
}

#[test]
fn through_sudo_worktrees_belong_to_and_run_as_the_user_who_ran_it() {
    let Some(f) = Fixture::new() else { return };
    let Some(user) = sudo_user() else { return };
    let dir = f.home.join("mnt");
    fs::create_dir(&dir).unwrap();
    f.give_to(user);

    let wt = succeed(f.sudo(&["worktrees", "create", "base"], user));
    succeed(f.sudo(&["worktrees", "mount", &wt, dir.to_str().unwrap()], user));
    succeed(f.sudo(&["worktrees", "unmount", &wt], user));
    let ids = succeed(f.sudo(
        &[
            "worktrees",
            "run",
            &wt,
            "--",
            "sh",
            "-c",
            "mkdir -p made/by/run && echo x > made/by/run/f && id -u && id -g",
        ],
        user,
    ));
    assert_eq!(ids, format!("{}\n{}", user.0, user.1));
    for path in walk(&f.store) {
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), user, "{}", path.display());
    }

    succeed(f.as_user(&["snapshots", "create", "--worktree", &wt], user));
    succeed(f.as_user(&["worktrees", "delete", &wt], user));
    succeed(f.as_user(&["trash", "empty", "--yes"], user));
    assert!(f.entries("worktrees").is_empty());
    assert!(f.entries("trash").is_empty());
}

#[test]
fn through_sudo_a_worktree_is_never_mounted_through_a_link() {
    let Some(f) = Fixture::new() else { return };
    let Some(user) = sudo_user() else { return };
    f.give_to(user);
    let wt = succeed(f.sudo(&["worktrees", "create", "base"], user));
    let victim = f.home.with_file_name("victim");
    fs::create_dir(&victim).unwrap();
    let dir = f.store.join("worktrees").join(&wt);
    let run = |script: &str| {
        let mut command = f.sudo(&["worktrees", "run", &wt, "--json"], user);
        command.args(["--", "sh", "-c", script]);
        command
    };

    fs::rename(dir.join("upper"), dir.join("kept")).unwrap();
    symlink(&victim, dir.join("upper")).unwrap();
    refuse(run("echo written > through-upper"));
    fs::remove_file(dir.join("upper")).unwrap();
    fs::rename(dir.join("kept"), dir.join("upper")).unwrap();

    let _ = fs::remove_file(dir.join("lock"));
    symlink(victim.join("through-lock"), dir.join("lock")).unwrap();
    refuse(run("true"));
    fs::remove_file(dir.join("lock")).unwrap();
    assert!(fs::read_dir(&victim).unwrap().next().is_none());

    std::os::unix::fs::chown(&f.project, Some(0), Some(0)).unwrap();
    assert_eq!(refuse(run("true"))["reason"], "permission_denied");
    std::os::unix::fs::chown(&f.project, Some(user.0), Some(user.1)).unwrap();
    assert_eq!(succeed(run("cat README.md")), "readme");
}
