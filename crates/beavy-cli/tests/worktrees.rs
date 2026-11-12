//! Worktrees end to end: the beavy binary against a real store and real overlay
//! mounts, as root or in a user namespace, whichever this process has.
//!
//! On a machine that can't mount worktrees these tests skip, unless
//! BEAVY_REQUIRE_WORKTREES is set, as CI sets it. BEAVY_EXPECT_PRIVILEGE (`root`
//! or `userns`) makes them fail when beavy mounts the other way.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
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
            "echo shared > marker && sleep 3",
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
    assert_eq!(f.sh(&wt, "cat marker"), "shared\n");

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
    assert_eq!(f.sh(&wt, "cat marker"), "shared\n");
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
