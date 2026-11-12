# CLI contract

This is the contract every `beavy` command keeps, for the person at the terminal and
for the agent or script that calls it. It follows the
[Vercel CLI Design Guidelines](https://vercel.com/design/cli); where this file is
silent, the guide applies. Anything a script can depend on is listed here: streams,
exit codes, JSON shapes, reason codes, and environment variables. Changing any of
them is a contract change (see [Changing the contract](#changing-the-contract)).

The shared code that enforces it lives in the `beavy` library crate:
`outcome.rs` (exit codes), `error.rs` (errors and reasons), `envelope.rs` (JSON
documents), and `output.rs` (mode resolution and rendering). Commands use those
types and never write to stdout or stderr, check terminals, or read mode variables
themselves.

## Streams

- **stdout holds only the requested result.** For a query that's its data; with
  `--json` it's exactly one JSON document followed by a newline.
- **stderr holds everything else:** context, warnings, errors, and next steps.
- With `--json`, stderr stays empty on success. On failure stdout stays empty and
  stderr holds exactly one single-line JSON error document.
- A closed stdout (`beavy host check | head -1`) ends the command quietly with exit 0.

## Modes

Each invocation resolves four settings independently, once, in `output::Mode`.

| Setting | Values | Decided by |
| --- | --- | --- |
| Format | human, JSON | JSON with `--json`, and only then |
| Interaction | interactive, non-interactive | Non-interactive with `--non-interactive`, `--json`, a nonempty `CI`, a detected agent, or when stdin or stderr isn't a terminal |
| Presentation | rich, plain | Per stream: rich only on a terminal whose `TERM` isn't `dumb` |
| Color | on, off | `--no-color`, then a nonempty `NO_COLOR`, then `FORCE_COLOR` (`0` or `false` means off), then on exactly when the stream is rich |

- Complete input behaves the same with or without a terminal. Piping stdout changes
  only stdout's presentation: it never selects JSON and never turns prompts off.
- Prompts need an interactive run. A non-interactive run that's missing input fails
  at once with `missing_argument` (exit 2) instead of waiting. The only prompt today
  is the confirmation in `beavy trash empty`; `--yes` answers it, and a
  non-interactive run without `--yes` fails with `confirmation_required` (exit 2).
- An agent is detected by any of `AI_AGENT`, `CLAUDECODE`, `CODEX_SANDBOX`,
  `CURSOR_AGENT`, or `GEMINI_CLI` being set and nonempty. Detection only turns
  prompts off; the agent still gets human output unless it passes `--json`.
- JSON is never styled, even when color is forced. It's indented with 2 spaces when
  stdout is a terminal and compact otherwise. Consumers must not depend on
  whitespace or key order.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The promised operation completed: a result, an empty result, a no-op, help, or the version |
| 1 | The input was accepted and the operation failed |
| 2 | Usage error or missing input: nothing ran |
| 130 | Interrupted with Ctrl+C (SIGINT), or a confirmation was declined |
| 128 + n | Terminated by signal n, such as 143 for SIGTERM |

The error's reason fixes its class and the class fixes the exit code, so the exit
code is the source of truth even when output is lost. beavy doesn't install signal
handlers yet, so 130 and 128 + n come from the default signal disposition.

## JSON results

Every `--json` document carries `schemaVersion`, the version of the envelope and of
every result shape inside it. It's `1` today and increases only with a breaking
change to a documented field. Adding a field isn't breaking.

A result is keyed by its noun, so top-level fields can be added safely:

```json
{
  "schemaVersion": 1,
  "host": { "...": "..." },
  "warnings": [{ "reason": "cleanup_failed", "message": "..." }],
  "next": [{ "command": "beavy host check --cwd <DIR>", "argv": ["beavy", "host", "check", "--cwd", "<DIR>"], "when": "..." }]
}
```

| Field | Rule |
| --- | --- |
| `schemaVersion` | Always present. An integer |
| `<noun>` | The result. Its shape is documented with its command |
| `warnings` | Present only when there are warnings. `reason` is a stable code from the [warning registry](#warning-reasons); `message` is human copy and may change |
| `next` | Present only when there's a next step. `command` is for display; `argv` is the exact argument vector to run; `when`, if present, says when to run it or how to fill a `<PLACEHOLDER>` |

Inside a result, an empty list is `[]` and a known-absent or unknown value is
`null`; documented fields are never omitted.

`--version --json` prints `{ "schemaVersion": 1, "version": "<VERSION>" }`.

## JSON errors

On failure with `--json`, stderr gets one line:

```json
{"schemaVersion":1,"status":"error","reason":"unknown_option","message":"Unknown option --bogus for beavy host check.","cause":null,"next":[{"command":"beavy host check --help","argv":["beavy","host","check","--help"]}]}
```

| Field | Rule |
| --- | --- |
| `schemaVersion` | Always present |
| `status` | `action_required` when the command was blocked before any effect and supplying input would let it run; `error` otherwise |
| `reason` | A stable snake_case code from the [reason registry](#error-reasons). Branch on this |
| `message` | The human headline. Its wording can change; never parse it |
| `cause` | The underlying error as text, or `null` |
| `next` | Present only when there's one exact command that moves the caller forward. Same shape as a result's `next` |

## Error reasons

| Reason | Class | Exit | Status | When |
| --- | --- | --- | --- | --- |
| `invalid_arguments` | Usage | 2 | `error` | A value or argument is malformed, or a path names the wrong kind of thing |
| `unknown_option` | Usage | 2 | `error` | An option isn't defined for the command |
| `unknown_command` | Usage | 2 | `error` | A command path segment doesn't exist |
| `missing_argument` | Missing input | 2 | `action_required` | Required input is missing and the run can't prompt for it |
| `confirmation_required` | Missing input | 2 | `action_required` | A command that destroys data needs `--yes` in a non-interactive run |
| `not_found` | Not found | 1 | `error` | A named file, directory, snapshot, or trash entry doesn't exist |
| `permission_denied` | Permission | 1 | `error` | The process isn't allowed to read or change what was named |
| `name_taken` | Conflict | 1 | `error` | Another snapshot in the store has the name |
| `snapshot_in_use` | Conflict | 1 | `error` | A snapshot can't be deleted while another snapshot was made from it |
| `unsupported_git_state` | Conflict | 1 | `error` | The project is a linked Git worktree or submodule, or its repository is in the middle of a merge, rebase, cherry-pick, revert, or bisect |
| `io_error` | System | 1 | `error` | The operating system failed a read or write |
| `invalid_store` | System | 1 | `error` | The store has a layout version this beavy can't read, or a damaged record |
| `backend_unavailable` | System | 1 | `error` | This platform can't keep snapshots yet: beavy's store only runs on Linux |
| `internal_error` | Internal | 1 | `error` | A bug in beavy |
| `canceled` | Canceled | 130 | `error` | The user declined a confirmation; nothing changed |

A code never changes meaning once published. New codes are added to this table and
to `error::Reason` in the same change.

## Warning reasons

| Reason | When |
| --- | --- |
| `cleanup_failed` | A temporary file or directory beavy created couldn't be removed. The message names the path |
| `entries_skipped` | An import left out entries it can't copy: sockets, and device nodes when beavy isn't root. The message names them |

## Human output

Human errors go to stderr, lead with what failed and the known cause, and end with
one exact command when there is one:

```
✗ Unknown option --bogus for beavy host check.

  See all options:
    beavy host check --help
```

- Rich output (a terminal) puts a glyph or a space in column 0 and content from
  column 2. Label/value rows start the value at column 18. Glyphs: `✓` the
  completed result, `!` a warning, `✗` an error.
- Plain output (a pipe, or `TERM=dumb`) has no gutter and uses words instead of
  glyphs: `Warning:` and `Error:`. Tables are tab-separated with a header row and
  are never padded or truncated, so `cut`, `grep`, and agents can read them.
- A next command prints alone on its own line, indented 4 spaces, with no `$` and no
  backticks. Stripping color recovers it exactly.

## Help and version

- `beavy --help`, `beavy -h`, `beavy help`, and bare `beavy` print root help;
  `beavy <path> --help` and `beavy help <path>` print the same help at any depth. A
  group with no action, such as `beavy host`, prints its help.
- Help goes to stdout, exits 0, and does no work: no config, no daemon, no network.
  `--help` wins over every other option.
- `-v` and `--version` print only the version at any depth.
- Every global option works before or after the command path: `--cwd <DIR>`,
  `-h, --help`, `--json`, `--no-color`, `--non-interactive`, `-v, --version`.

Not built yet: machine-readable help (`--help --json`), `help search`, and
`--fields`. Until they exist, they aren't part of the contract.

## Environment variables

| Variable | Effect |
| --- | --- |
| `BEAVY_STORE` | The store directory when `--store` isn't given. Empty counts as unset |
| `XDG_DATA_HOME`, `HOME` | Where default stores live (see [Stores](#stores)) |
| `NO_COLOR`, `FORCE_COLOR`, `TERM`, `CI`, agent variables | See [Modes](#modes) |

## Stores

Snapshots live in a store: an ordinary directory that beavy creates
on first use and never writes outside of. Deleting a project's store removes every
trace of beavy from it.

A `snapshots` or `trash` command picks its store in this order:

1. `--store <DIR>`.
2. `BEAVY_STORE`.
3. The project's default store, `$XDG_DATA_HOME/beavy/stores/<name>-<hash>`, or
   `~/.local/share/beavy/stores/<name>-<hash>` when `XDG_DATA_HOME` isn't set.
   `<name>` is the project directory's name and `<hash>` is derived from its path,
   so the default store is always outside the project.

The project is the nearest directory at or above `--cwd` that already has a default
store, else the root of the Git working tree around it, else `--cwd` itself. Every
suggested command repeats the `--cwd` and `--store` the caller passed.

Reads never create a store; the first change does. Every change takes the store's
lock and first repairs whatever an interrupted command left behind, so a killed
command never leaves a half-made snapshot in view. A store records its
layout version, and a beavy that can't read it fails with `invalid_store`.
Snapshots need Linux; on other platforms these commands fail with
`backend_unavailable`.

## Commands

### beavy host check

Check which worktree backends this machine can support for a directory.

```
beavy host check [--cwd <DIR>] [--json]
```

`host` is the namespace for the machine beavy runs on, and `check` is the guide's
verb for running checks and reporting each result. The directory is `--cwd`, or the
current directory.

Every status comes from evidence gathered on this machine, and a status is
`unknown` whenever the evidence doesn't settle it:

| Check | How it's decided |
| --- | --- |
| `fileClone` | Writes a file in a temporary directory inside the checked directory and clones it: `FICLONE` on Linux, `clonefile` on macOS |
| `overlayfs` | A successful probe mount, an ENODEV from the kernel, `/proc/filesystems`, or a loaded `overlay` module |
| `overlayfsMount` | Mounts overlayfs in the temporary directory from a short-lived child process: in a private mount namespace when the process has `CAP_SYS_ADMIN`, in an unprivileged user namespace otherwise. The mount never leaves the child |
| `userNamespaces` | A child process creates a user namespace and maps the current user to root in it |
| `fuse` | `/dev/fuse` exists, is a character device, and opens for reading and writing |

The command never needs root. It removes its temporary directory before exiting;
if it can't, it says so with a `cleanup_failed` warning naming the path. A
directory that doesn't exist fails with `not_found` (exit 1), and a file instead of
a directory fails with `invalid_arguments` (exit 2).

Result:

```json
{
  "schemaVersion": 1,
  "host": {
    "os": { "name": "linux", "version": "6.1.158", "kernel": "6.1.158", "arch": "x86_64" },
    "directory": { "path": "/work/acme", "filesystem": "ext4", "writable": true },
    "root": false,
    "checks": {
      "fileClone": { "status": "unsupported", "reason": "ext4 doesn't support reflinks: FICLONE failed with Operation not supported (os error 95)." },
      "overlayfs": { "status": "supported", "reason": "Mounted overlayfs in a probe directory." },
      "overlayfsMount": { "status": "supported", "reason": "Mounted overlayfs on ext4 in an unprivileged user namespace." },
      "userNamespaces": { "status": "supported", "reason": "Created a user namespace and mapped this user to root in it." },
      "fuse": { "status": "supported", "reason": "Opened /dev/fuse for reading and writing." }
    },
    "backends": [
      { "name": "overlayfs", "status": "supported", "reason": "Mounted overlayfs on ext4 in an unprivileged user namespace." },
      { "name": "clonefile", "status": "unsupported", "reason": "clonefile is only available on macOS." },
      { "name": "reflink", "status": "unsupported", "reason": "ext4 doesn't support reflinks: FICLONE failed with Operation not supported (os error 95)." },
      { "name": "fuse", "status": "supported", "reason": "Opened /dev/fuse for reading and writing." },
      { "name": "copy", "status": "supported", "reason": "Copies every file, so it works on any writable filesystem." }
    ]
  }
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `host.os.name` | `"linux"` or `"macos"` | Operating system |
| `host.os.version` | string or null | Kernel release on Linux, product version (such as `15.1`) on macOS |
| `host.os.kernel` | string or null | Kernel release, as `uname -r` prints it |
| `host.os.arch` | string | CPU architecture, such as `x86_64` or `aarch64` |
| `host.directory.path` | string | Canonical path of the checked directory |
| `host.directory.filesystem` | string or null | Filesystem type, such as `ext4`, `btrfs`, `overlay`, or `apfs` |
| `host.directory.writable` | boolean or null | Whether beavy could create files in the directory |
| `host.root` | boolean | The effective user is root |
| `host.checks.<check>.status` | `"supported"`, `"unsupported"`, or `"unknown"` | See the checks table |
| `host.checks.<check>.reason` | string | The evidence, as a sentence. Human copy; don't parse it |
| `host.backends` | array | Every backend, always in this order: `overlayfs`, `clonefile`, `reflink`, `fuse`, `copy` |
| `host.backends[].status` | `"supported"`, `"unsupported"`, or `"unknown"` | Whether the backend can work for this directory on this host |

Human output prints the facts as label/value rows, then the backends as a table,
on stdout.

### beavy snapshots create

Import the project into the store as a new snapshot.

```
beavy snapshots create [--name <NAME>] [--store <DIR>] [--json]
```

The command copies the project into the store once. It clones
files with `FICLONE` when the filesystem supports reflinks and copies them
otherwise. It keeps modes, times, symlinks, FIFOs, and hard links, and ownership
only as root. It skips sockets and device nodes with an `entries_skipped` warning.
The project itself is only read, and never becomes part of a mount. In the copy,
Git's HEAD is detached at the current commit, with the index and working tree as
they were. A linked Git worktree or submodule fails with `unsupported_git_state`
and a next command for the main working tree. A repository in the middle of a
merge, rebase, cherry-pick, revert, or bisect, or locked by a running git, fails
the same way.

A name is 1 to 63 lowercase letters, digits, and hyphens, starting and ending with a
letter or digit, and unique among the store's snapshots (`name_taken`). The
snapshot ID prints alone on stdout; everything else goes to stderr.

Result:

```json
{ "schemaVersion": 1, "snapshot": { "...": "..." }, "outcome": "created", "import": { "files": 388848, "bytes": 3812664320 }, "store": { "path": "/home/me/.local/share/beavy/stores/acme-3f9a1c2e" } }
```

A snapshot object:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | `snap_` and 10 characters |
| `name` | string or null | |
| `parent` | string or null | The snapshot this one was made from; null for an import |
| `source` | string | The directory the chain's base was imported from |
| `commit` | string or null | The Git commit checked out when the snapshot was taken |
| `method` | `"copy"` or `"reflink"` | How the top layer was made |
| `depth` | integer | Layers in the chain, the base included |
| `createdAt` | string | RFC 3339 UTC time |
| `children` | array of strings | Snapshots whose parent is this one |

### beavy snapshots list

```
beavy snapshots list [--store <DIR>] [--json]
```

Result: `{ "snapshots": [<snapshot>], "store": { "path": "..." } }`, newest first.
Human output is a table of ID, Name, Parent, Depth, and Created on stdout.

### beavy snapshots inspect

```
beavy snapshots inspect <SNAPSHOT> [--store <DIR>] [--json]
```

`<SNAPSHOT>` is an ID or a name. Result: `{ "snapshot": <snapshot>, "store": { "path": "..." } }`.
An unknown snapshot fails with `not_found`.

### beavy snapshots delete

```
beavy snapshots delete <SNAPSHOT> [--store <DIR>] [--json]
```

Moves the snapshot to the trash; its data stays on disk until
`beavy trash empty`. Fails with `snapshot_in_use` while another snapshot was made
from it. Result: `{ "snapshot": <snapshot>, "outcome": "deleted", "store": { "path": "..." } }`,
with next commands to list snapshots and to restore this one.

### beavy trash list

```
beavy trash list [--store <DIR>] [--json]
```

Result: `{ "entries": [{ "id": "...", "kind": "snapshot", "name": null, "deletedAt": "..." }], "store": { "path": "..." } }`,
most recently deleted first. `kind` is `"snapshot"`.

### beavy trash restore

```
beavy trash restore <ID> [--store <DIR>] [--json]
```

Puts a deleted snapshot back under its old ID. Fails with `name_taken` when a live
snapshot has its name now. Result: `{ "entry": <entry>, "outcome": "restored", "store": { "path": "..." } }`.

### beavy trash empty

```
beavy trash empty [-y, --yes] [--store <DIR>] [--json]
```

Deletes everything in the trash for good, with the layers no live snapshot uses.
An interactive run asks first; declining exits 130 with `canceled`. A
non-interactive run needs `--yes`, and without it fails with
`confirmation_required` (exit 2) and a next command whose `when` says to run it
only after a human approves. Result: `{ "entries": [<entry>], "outcome": "emptied", "store": { "path": "..." } }`,
or `"outcome": "unchanged"` when the trash was already empty.

## Changing the contract

| Change | Requirement |
| --- | --- |
| Human copy or layout | Updated tests |
| Add a command, option, JSON field, reason, or warning | Tests for the new behavior and for unchanged old invocations, and this file |
| Rename a command or option | The old name stays as a deprecated alias for at least one release |
| Change a meaning, default, type, stream, exit code, or JSON shape, or remove anything | A `schemaVersion` bump for JSON shapes, release notes naming the replacement, and this file |
