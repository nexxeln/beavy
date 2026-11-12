//! Runs the `beavy` binary and asserts on stdout, stderr, and the exit code
//! separately, in human and JSON modes.

use std::io::Read;
use std::process::{Command, Stdio};

use serde_json::Value;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn beavy() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_beavy"));
    for var in ["NO_COLOR", "FORCE_COLOR", "TERM"] {
        command.env_remove(var);
    }
    command.stdin(Stdio::null());
    command
}

fn run(args: &[&str]) -> Run {
    run_command(beavy().args(args))
}

fn run_command(command: &mut Command) -> Run {
    let output = command.output().expect("beavy runs");
    Run {
        code: output.status.code().expect("beavy exits with a code"),
        stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

/// Exactly one JSON document and a newline.
fn one_document(text: &str) -> Value {
    assert!(
        text.ends_with('\n'),
        "document ends with a newline: {text:?}"
    );
    assert_eq!(text.lines().count(), 1, "one line: {text:?}");
    serde_json::from_str(text).expect("valid JSON")
}

const BACKENDS: [&str; 5] = ["overlayfs", "clonefile", "reflink", "fuse", "copy"];
const STATUSES: [&str; 3] = ["supported", "unsupported", "unknown"];

#[test]
fn host_check_human_piped_writes_the_report_to_stdout_only() {
    let dir = tempfile::tempdir().unwrap();
    let run = run(&["host", "check", "--cwd", dir.path().to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stderr, "");
    assert!(!run.stdout.contains('\x1b'), "no ANSI when piped");
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(lines[0].starts_with("OS              "), "{}", run.stdout);
    let canonical = dir.path().canonicalize().unwrap();
    assert!(
        run.stdout
            .contains(&format!("Directory       {}\n", canonical.display()))
    );
    let header = lines
        .iter()
        .position(|l| *l == "Backend\tStatus\tReason")
        .expect("table header");
    let rows = &lines[header + 1..];
    assert_eq!(rows.len(), BACKENDS.len());
    for (row, name) in rows.iter().zip(BACKENDS) {
        let cells: Vec<&str> = row.split('\t').collect();
        assert_eq!(cells.len(), 3, "{row}");
        assert_eq!(cells[0], name);
        assert!(STATUSES.contains(&cells[1]), "{row}");
        assert!(!cells[2].is_empty());
    }
}

#[test]
fn host_check_json_is_one_document_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec![
            "host",
            "check",
            "--json",
            "--cwd",
            dir.path().to_str().unwrap(),
        ],
        vec![
            "--json",
            "--cwd",
            dir.path().to_str().unwrap(),
            "host",
            "check",
        ],
    ] {
        let run = run(&args);
        assert_eq!(run.code, 0, "{}", run.stderr);
        assert_eq!(run.stderr, "", "stderr stays empty in JSON mode");
        let doc = one_document(&run.stdout);
        assert_eq!(doc["schemaVersion"], 1);
        let host = &doc["host"];
        assert!(["linux", "macos"].contains(&host["os"]["name"].as_str().unwrap()));
        assert_eq!(
            host["directory"]["path"],
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(host["directory"]["writable"], true);
        assert!(host["root"].is_boolean());
        for key in [
            "fileClone",
            "overlayfs",
            "overlayfsMount",
            "userNamespaces",
            "fuse",
        ] {
            let status = host["checks"][key]["status"].as_str().unwrap();
            assert!(STATUSES.contains(&status), "{key}: {status}");
            assert!(host["checks"][key]["reason"].is_string());
        }
        let names: Vec<&str> = host["backends"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, BACKENDS);
        assert!(doc.get("warnings").is_none());
    }
}

#[test]
fn host_check_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    for json in [false, true] {
        let mut args = vec!["host", "check", "--cwd", dir.path().to_str().unwrap()];
        if json {
            args.push("--json");
        }
        assert_eq!(run(&args).code, 0);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
}

#[test]
fn host_check_reports_the_platform_backend_it_can_test() {
    let run = run(&["host", "check", "--json"]);
    let doc = one_document(&run.stdout);
    let backends = doc["host"]["backends"].as_array().unwrap();
    let status = |name: &str| {
        backends
            .iter()
            .find(|b| b["name"] == name)
            .map(|b| b["status"].as_str().unwrap().to_string())
            .unwrap()
    };
    if cfg!(target_os = "macos") {
        assert_eq!(status("overlayfs"), "unsupported");
        assert_eq!(status("reflink"), "unsupported");
        assert_eq!(
            doc["host"]["checks"]["userNamespaces"]["status"],
            "unsupported"
        );
    } else {
        assert_eq!(status("clonefile"), "unsupported");
    }
    assert!(doc["host"]["os"]["kernel"].is_string());
}

#[test]
fn missing_directory_fails_with_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let missing = missing.to_str().unwrap();

    let human = run(&["host", "check", "--cwd", missing]);
    assert_eq!(human.code, 1);
    assert_eq!(human.stdout, "");
    assert!(
        human
            .stderr
            .starts_with(&format!("Error: Couldn't find directory {missing}.\n")),
        "{}",
        human.stderr
    );

    let json = run(&["host", "check", "--cwd", missing, "--json"]);
    assert_eq!(json.code, 1);
    assert_eq!(json.stdout, "");
    let doc = one_document(&json.stderr);
    assert_eq!(doc["schemaVersion"], 1);
    assert_eq!(doc["status"], "error");
    assert_eq!(doc["reason"], "not_found");
    assert_eq!(
        doc["message"],
        format!("Couldn't find directory {missing}.")
    );
    assert!(doc["cause"].is_string());
    assert!(doc.get("next").is_none());
}

#[test]
fn a_file_is_a_usage_error() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let run = run(&[
        "host",
        "check",
        "--json",
        "--cwd",
        file.path().to_str().unwrap(),
    ]);
    assert_eq!(run.code, 2);
    assert_eq!(run.stdout, "");
    assert_eq!(one_document(&run.stderr)["reason"], "invalid_arguments");
}

#[test]
fn unknown_option_fails_with_exit_2_and_the_help_command() {
    let human = run(&["host", "check", "--bogus"]);
    assert_eq!(human.code, 2);
    assert_eq!(human.stdout, "");
    assert_eq!(
        human.stderr,
        "Error: Unknown option --bogus for beavy host check.\n\n  See all options:\n    beavy host check --help\n"
    );

    let json = run(&["host", "check", "--bogus", "--json"]);
    assert_eq!(json.code, 2);
    assert_eq!(json.stdout, "");
    let doc = one_document(&json.stderr);
    assert_eq!(doc["reason"], "unknown_option");
    assert_eq!(doc["status"], "error");
    assert_eq!(doc["next"][0]["command"], "beavy host check --help");
    assert_eq!(
        doc["next"][0]["argv"],
        serde_json::json!(["beavy", "host", "check", "--help"])
    );
}

#[test]
fn unknown_command_suggests_the_closest_one() {
    let human = run(&["host", "chek"]);
    assert_eq!(human.code, 2);
    assert_eq!(human.stdout, "");
    assert!(
        human
            .stderr
            .starts_with("Error: Unknown command chek for beavy host. Did you mean check?\n")
    );

    let json = run(&["--json", "frob"]);
    assert_eq!(json.code, 2);
    assert_eq!(json.stdout, "");
    let doc = one_document(&json.stderr);
    assert_eq!(doc["reason"], "unknown_command");
    assert_eq!(doc["next"][0]["command"], "beavy --help");
}

#[test]
fn help_works_everywhere_with_no_environment() {
    let leaf = run(&["host", "check", "--help"]).stdout;
    assert!(leaf.starts_with("Usage: beavy host check [OPTIONS]\n"));
    assert!(leaf.contains("Examples:"));
    for (args, expected) in [
        (vec!["help", "host", "check"], leaf.clone()),
        (vec!["host", "check", "-h"], leaf.clone()),
        (vec!["host", "check", "--json", "--help"], leaf.clone()),
        (vec!["help"], run(&["--help"]).stdout),
        (vec![], run(&["--help"]).stdout),
        (vec!["host"], run(&["host", "--help"]).stdout),
    ] {
        let run = run_command(beavy().env_clear().args(&args));
        assert_eq!(run.code, 0, "{args:?}");
        assert_eq!(run.stderr, "", "{args:?}");
        assert_eq!(run.stdout, expected, "{args:?}");
    }
    let root = run(&[]).stdout;
    assert!(root.contains("Snapshots:\n  snapshots  Create, inspect, and delete snapshots"));
    assert!(root.contains("Machine:\n  host       Inspect this machine"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn snapshots_and_trash_report_the_backend_unavailable() {
    for group in ["snapshots", "trash"] {
        let run = run(&[group, "list", "--json"]);
        assert_eq!(run.code, 1, "{group}");
        assert_eq!(run.stdout, "");
        let doc = one_document(&run.stderr);
        assert_eq!(doc["reason"], "backend_unavailable");
        assert_eq!(doc["next"][0]["command"], "beavy host check");
    }
}

#[test]
fn version_prints_only_the_version() {
    let human = run(&["--version"]);
    assert_eq!(human.code, 0);
    assert_eq!(human.stdout, format!("{}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(human.stderr, "");
    assert_eq!(run(&["host", "check", "-v"]).stdout, human.stdout);

    let json = run(&["host", "check", "--version", "--json"]);
    assert_eq!(json.code, 0);
    assert_eq!(json.stderr, "");
    assert_eq!(
        one_document(&json.stdout),
        serde_json::json!({ "schemaVersion": 1, "version": env!("CARGO_PKG_VERSION") })
    );
}

#[test]
fn json_and_identifiers_stay_unstyled_when_color_is_forced() {
    let json = run_command(
        beavy()
            .env("FORCE_COLOR", "1")
            .args(["host", "check", "--json"]),
    );
    assert!(!json.stdout.contains('\x1b'));
    let err = run_command(
        beavy()
            .env("FORCE_COLOR", "1")
            .args(["host", "check", "--bogus", "--json"]),
    );
    assert!(!err.stderr.contains('\x1b'));

    let human = run_command(
        beavy()
            .env("FORCE_COLOR", "1")
            .args(["host", "check", "--bogus"]),
    );
    assert!(
        human
            .stderr
            .starts_with("\x1b[31mError:\x1b[0m Unknown option --bogus")
    );
    let no_color = run_command(beavy().env("FORCE_COLOR", "1").args([
        "host",
        "check",
        "--bogus",
        "--no-color",
    ]));
    assert!(!no_color.stderr.contains('\x1b'));
}

#[test]
fn piped_stdin_changes_nothing() {
    let null = run(&["host", "check", "--json"]);
    let piped = run_command(
        beavy()
            .stdin(Stdio::piped())
            .args(["host", "check", "--json"]),
    );
    assert_eq!(null.code, piped.code);
    let (a, b) = (one_document(&null.stdout), one_document(&piped.stdout));
    assert_eq!(a["host"]["backends"], b["host"]["backends"]);
}

#[test]
fn a_closed_stdout_ends_quietly() {
    let mut child = beavy()
        .args(["host", "check"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert_eq!(stderr, "");
}

/// Runs beavy with stdout on a pseudo-terminal and stderr piped, and returns
/// (exit code, stdout as the terminal received it, stderr).
fn run_on_terminal(args: &[&str]) -> (i32, String, String) {
    use std::os::fd::{FromRawFd, OwnedFd};

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

    let child = beavy()
        .env("TERM", "xterm")
        .args(args)
        .stdout(Stdio::from(terminal))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let reader = std::thread::spawn(move || {
        let mut file = std::fs::File::from(controller);
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&buf[..n]),
            }
        }
        bytes
    });
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(reader.join().unwrap())
        .unwrap()
        .replace("\r\n", "\n");
    (
        output.status.code().unwrap(),
        stdout,
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn on_a_terminal_json_is_indented_and_tables_are_aligned() {
    let (code, stdout, stderr) = run_on_terminal(&["host", "check", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "");
    assert!(
        stdout.starts_with("{\n  \"schemaVersion\": 1,\n"),
        "{stdout:?}"
    );
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        doc["host"]["backends"].as_array().unwrap().len(),
        BACKENDS.len()
    );

    let (code, stdout, stderr) = run_on_terminal(&["host", "check"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains('\t'), "{stdout:?}");
    assert!(stdout.starts_with("  OS              "), "{stdout:?}");
    assert!(
        stdout.contains("\n  Backend     Status        Reason\n"),
        "{stdout:?}"
    );
}
