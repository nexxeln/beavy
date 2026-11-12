//! Mode resolution, stream discipline, and rendering.
//!
//! stdout carries only the requested result. Everything else (context, warnings,
//! errors, next steps) goes to stderr. In JSON mode stdout holds exactly one
//! document and stderr stays empty unless the command fails, in which case it
//! holds exactly one single-line error document.

use std::io::{self, IsTerminal, Write};

use serde::Serialize;

use crate::envelope::{Document, ErrorDocument, Warning};
use crate::error::Error;

/// Options from the command line that affect the mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeOptions {
    pub json: bool,
    pub non_interactive: bool,
    pub no_color: bool,
}

/// Which standard streams are terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminals {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Terminals {
    pub fn detect() -> Self {
        Terminals {
            stdin: io::stdin().is_terminal(),
            stdout: io::stdout().is_terminal(),
            stderr: io::stderr().is_terminal(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

/// How one stream is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// The stream is a terminal.
    pub terminal: bool,
    /// Glyphs and aligned layout: a terminal whose `TERM` isn't `dumb`.
    pub rich: bool,
    /// ANSI color is allowed.
    pub color: bool,
}

/// The resolved settings for one invocation. Commands read these and never check
/// terminals or environment variables themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub format: Format,
    /// Prompts are allowed. Missing input fails fast when this is false.
    pub interactive: bool,
    pub stdout: Presentation,
    pub stderr: Presentation,
}

/// Environment variables set by agent harnesses. A detected agent never gets a prompt.
const AGENT_VARIABLES: &[&str] = &[
    "AI_AGENT",
    "CLAUDECODE",
    "CODEX_SANDBOX",
    "CURSOR_AGENT",
    "GEMINI_CLI",
];

impl Mode {
    /// Resolve from the real terminals and process environment.
    pub fn detect(options: ModeOptions) -> Self {
        Mode::resolve(options, Terminals::detect(), |name| {
            std::env::var(name).ok()
        })
    }

    /// Resolve from explicit inputs; `env` looks up an environment variable.
    pub fn resolve(
        options: ModeOptions,
        terminals: Terminals,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
        let agent = AGENT_VARIABLES.iter().any(|name| set(name));
        let interactive = !(options.non_interactive
            || options.json
            || set("CI")
            || agent
            || !terminals.stdin
            || !terminals.stderr);

        let dumb = env("TERM").is_some_and(|term| term == "dumb");
        let color_preference = if options.no_color || set("NO_COLOR") {
            Some(false)
        } else {
            env("FORCE_COLOR").map(|value| value != "0" && value != "false")
        };
        let presentation = |terminal: bool| {
            let rich = terminal && !dumb;
            Presentation {
                terminal,
                rich,
                color: color_preference.unwrap_or(rich),
            }
        };

        Mode {
            format: if options.json {
                Format::Json
            } else {
                Format::Human
            },
            interactive,
            stdout: presentation(terminals.stdout),
            stderr: presentation(terminals.stderr),
        }
    }
}

/// The two output streams of one invocation, bound to its mode.
pub struct Output<O, E> {
    mode: Mode,
    stdout: O,
    stderr: E,
}

impl Output<io::Stdout, io::Stderr> {
    pub fn stdio(mode: Mode) -> Self {
        Output::new(mode, io::stdout(), io::stderr())
    }
}

impl<O: Write, E: Write> Output<O, E> {
    pub fn new(mode: Mode, stdout: O, stderr: E) -> Self {
        Output {
            mode,
            stdout,
            stderr,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Write the result document to stdout: indented when stdout is a terminal,
    /// one line otherwise.
    pub fn json<T: Serialize>(&mut self, document: &Document<T>) -> io::Result<()> {
        let text = if self.mode.stdout.terminal {
            serde_json::to_string_pretty(document)
        } else {
            serde_json::to_string(document)
        }
        .map_err(io::Error::other)?;
        writeln!(self.stdout, "{text}")?;
        self.stdout.flush()
    }

    /// The human result stream.
    pub fn stdout(&mut self) -> Human<&mut O> {
        Human::new(&mut self.stdout, self.mode.stdout)
    }

    /// The human stream for context, warnings, and next steps. Writes nothing in
    /// JSON mode.
    pub fn stderr(&mut self) -> Human<Sink<&mut E>> {
        let sink = Sink {
            inner: &mut self.stderr,
            muted: self.mode.format == Format::Json,
        };
        Human::new(sink, self.mode.stderr)
    }

    /// Report warnings on stderr in human mode. JSON mode carries them in the document.
    pub fn warnings(&mut self, warnings: &[Warning]) -> io::Result<()> {
        let mut stderr = self.stderr();
        for warning in warnings {
            stderr.warning(&warning.message)?;
        }
        Ok(())
    }

    /// Report a failure on stderr: one single-line JSON document in JSON mode, the
    /// human error block otherwise.
    pub fn error(&mut self, err: &Error) -> io::Result<()> {
        if self.mode.format == Format::Json {
            let text =
                serde_json::to_string(&ErrorDocument::from(err)).map_err(io::Error::other)?;
            writeln!(self.stderr, "{text}")?;
        } else {
            Human::new(&mut self.stderr, self.mode.stderr).error(err)?;
        }
        self.stderr.flush()
    }
}

/// A writer that discards everything when muted.
pub struct Sink<W> {
    inner: W,
    muted: bool,
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.muted {
            Ok(buf.len())
        } else {
            self.inner.write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Width of the label column in label/value rows.
pub const LABEL_WIDTH: usize = 16;

/// Renders human output on one stream, rich or plain.
///
/// Rich output puts a glyph or a space in column 0, content from column 2, and
/// values of label/value rows from column 18. Plain output starts at column 0,
/// uses words instead of glyphs, and writes tables tab-separated.
pub struct Human<W> {
    w: W,
    presentation: Presentation,
}

const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

impl<W: Write> Human<W> {
    pub fn new(w: W, presentation: Presentation) -> Self {
        Human { w, presentation }
    }

    fn paint(&self, color: &str, text: &str) -> String {
        if self.presentation.color {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn indent(&self) -> &'static str {
        if self.presentation.rich { "  " } else { "" }
    }

    /// Text exactly as given, such as rendered help.
    pub fn raw(&mut self, text: &str) -> io::Result<()> {
        self.w.write_all(text.as_bytes())?;
        self.w.flush()
    }

    pub fn blank(&mut self) -> io::Result<()> {
        writeln!(self.w)
    }

    /// A line of prose in the content column.
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.w, "{}{text}", self.indent())
    }

    /// A label/value row. A label that fills the column puts the value on the next line.
    pub fn row(&mut self, label: &str, value: &str) -> io::Result<()> {
        let indent = self.indent();
        if label.chars().count() < LABEL_WIDTH {
            writeln!(self.w, "{indent}{label:<LABEL_WIDTH$}{value}")
        } else {
            writeln!(self.w, "{indent}{label}")?;
            writeln!(self.w, "{indent}{:LABEL_WIDTH$}{value}", "")
        }
    }

    /// A borderless table. Rich: aligned columns. Plain: header and rows separated by
    /// one tab, never padded or truncated.
    pub fn table(&mut self, header: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
        if !self.presentation.rich {
            writeln!(self.w, "{}", header.join("\t"))?;
            for row in rows {
                writeln!(self.w, "{}", row.join("\t"))?;
            }
            return Ok(());
        }
        let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
        for row in rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let mut write_row = |cells: &mut dyn Iterator<Item = &str>| {
            let mut line = String::from("  ");
            for (i, cell) in cells.enumerate() {
                line.push_str(cell);
                if i + 1 < widths.len() {
                    let pad = widths[i] - cell.chars().count() + 3;
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            writeln!(self.w, "{}", line.trim_end())
        };
        write_row(&mut header.iter().copied())?;
        for row in rows {
            write_row(&mut row.iter().map(String::as_str))?;
        }
        Ok(())
    }

    /// The one primary completed result.
    pub fn success(&mut self, text: &str) -> io::Result<()> {
        if self.presentation.rich {
            let glyph = self.paint(GREEN, "✓");
            writeln!(self.w, "{glyph} {text}")
        } else {
            writeln!(self.w, "{text}")
        }
    }

    pub fn warning(&mut self, text: &str) -> io::Result<()> {
        if self.presentation.rich {
            let glyph = self.paint(YELLOW, "!");
            writeln!(self.w, "{glyph} {text}")
        } else {
            let prefix = self.paint(YELLOW, "Warning:");
            writeln!(self.w, "{prefix} {text}")
        }
    }

    /// The error block: headline, cause, then the next command after a blank line.
    pub fn error(&mut self, err: &Error) -> io::Result<()> {
        if self.presentation.rich {
            let glyph = self.paint(RED, "✗");
            writeln!(self.w, "{glyph} {}", err.message())?;
        } else {
            let prefix = self.paint(RED, "Error:");
            writeln!(self.w, "{prefix} {}", err.message())?;
        }
        if let Some(cause) = err.cause() {
            writeln!(self.w, "  {:<LABEL_WIDTH$}{cause}", "Cause")?;
        }
        if let (Some(instruction), Some(next)) = (err.next_instruction(), err.next()) {
            let command = if self.presentation.rich {
                self.paint(CYAN, &next.command)
            } else {
                next.command.clone()
            };
            writeln!(self.w)?;
            writeln!(self.w, "  {instruction}:")?;
            writeln!(self.w, "    {command}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::error::{Next, Reason};

    const ALL_TTY: Terminals = Terminals {
        stdin: true,
        stdout: true,
        stderr: true,
    };
    const NO_TTY: Terminals = Terminals {
        stdin: false,
        stdout: false,
        stderr: false,
    };

    fn resolve(options: ModeOptions, terminals: Terminals, vars: &[(&str, &str)]) -> Mode {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Mode::resolve(options, terminals, |name| vars.get(name).cloned())
    }

    fn json() -> ModeOptions {
        ModeOptions {
            json: true,
            ..ModeOptions::default()
        }
    }

    #[test]
    fn a_terminal_session_is_interactive_and_rich() {
        let mode = resolve(ModeOptions::default(), ALL_TTY, &[]);
        assert_eq!(mode.format, Format::Human);
        assert!(mode.interactive);
        assert!(mode.stdout.rich && mode.stdout.color);
        assert!(mode.stderr.rich && mode.stderr.color);
    }

    #[test]
    fn json_ci_agents_and_missing_terminals_turn_prompts_off() {
        assert!(!resolve(json(), ALL_TTY, &[]).interactive);
        let non_interactive = ModeOptions {
            non_interactive: true,
            ..ModeOptions::default()
        };
        assert!(!resolve(non_interactive, ALL_TTY, &[]).interactive);
        assert!(!resolve(ModeOptions::default(), ALL_TTY, &[("CI", "true")]).interactive);
        assert!(!resolve(ModeOptions::default(), ALL_TTY, &[("CLAUDECODE", "1")]).interactive);
        let no_stdin = Terminals {
            stdin: false,
            ..ALL_TTY
        };
        assert!(!resolve(ModeOptions::default(), no_stdin, &[]).interactive);
        let no_stderr = Terminals {
            stderr: false,
            ..ALL_TTY
        };
        assert!(!resolve(ModeOptions::default(), no_stderr, &[]).interactive);
        assert!(resolve(ModeOptions::default(), ALL_TTY, &[("CI", "")]).interactive);
    }

    #[test]
    fn piping_stdout_changes_only_stdout_presentation() {
        let piped = Terminals {
            stdout: false,
            ..ALL_TTY
        };
        let mode = resolve(ModeOptions::default(), piped, &[]);
        assert_eq!(mode.format, Format::Human);
        assert!(mode.interactive);
        assert!(!mode.stdout.rich && !mode.stdout.color);
        assert!(mode.stderr.rich && mode.stderr.color);
    }

    #[test]
    fn color_follows_its_precedence() {
        let no_color = ModeOptions {
            no_color: true,
            ..ModeOptions::default()
        };
        assert!(
            !resolve(no_color, ALL_TTY, &[("FORCE_COLOR", "1")])
                .stderr
                .color
        );
        assert!(
            !resolve(ModeOptions::default(), ALL_TTY, &[("NO_COLOR", "1")])
                .stderr
                .color
        );
        assert!(
            resolve(ModeOptions::default(), ALL_TTY, &[("NO_COLOR", "")])
                .stderr
                .color
        );
        assert!(
            !resolve(ModeOptions::default(), ALL_TTY, &[("FORCE_COLOR", "0")])
                .stderr
                .color
        );
        let forced = resolve(ModeOptions::default(), NO_TTY, &[("FORCE_COLOR", "1")]);
        assert!(forced.stderr.color && !forced.stderr.rich);
        let dumb = resolve(ModeOptions::default(), ALL_TTY, &[("TERM", "dumb")]);
        assert!(!dumb.stderr.rich && !dumb.stderr.color && dumb.stderr.terminal);
    }

    fn render(presentation: Presentation, f: impl FnOnce(&mut Human<&mut Vec<u8>>)) -> String {
        let mut buf = Vec::new();
        f(&mut Human::new(&mut buf, presentation));
        String::from_utf8(buf).unwrap()
    }

    const RICH: Presentation = Presentation {
        terminal: true,
        rich: true,
        color: false,
    };
    const PLAIN: Presentation = Presentation {
        terminal: false,
        rich: false,
        color: false,
    };

    #[test]
    fn rows_put_values_at_column_18_in_rich_output() {
        let text = render(RICH, |h| {
            h.row("Filesystem", "ext4").unwrap();
            h.row("A Very Long Label", "x").unwrap();
        });
        assert_eq!(
            text,
            "  Filesystem      ext4\n  A Very Long Label\n                  x\n"
        );
        assert_eq!(text.lines().next().unwrap().find("ext4"), Some(18));
    }

    #[test]
    fn tables_align_when_rich_and_use_tabs_when_plain() {
        let rows = vec![
            vec!["overlayfs".to_string(), "supported".to_string()],
            vec!["copy".to_string(), "unknown".to_string()],
        ];
        let rich = render(RICH, |h| h.table(&["Backend", "Status"], &rows).unwrap());
        assert_eq!(
            rich,
            "  Backend     Status\n  overlayfs   supported\n  copy        unknown\n"
        );
        let plain = render(PLAIN, |h| h.table(&["Backend", "Status"], &rows).unwrap());
        assert_eq!(
            plain,
            "Backend\tStatus\noverlayfs\tsupported\ncopy\tunknown\n"
        );
    }

    #[test]
    fn errors_render_headline_cause_and_next() {
        let err = Error::new(
            Reason::UnknownOption,
            "Unknown option --x for beavy host check.",
        )
        .with_cause(io::Error::other("bad"))
        .with_next(
            "See all options",
            Next::new(["beavy", "host", "check", "--help"]),
        );
        let rich = render(RICH, |h| h.error(&err).unwrap());
        assert_eq!(
            rich,
            "✗ Unknown option --x for beavy host check.\n  Cause           bad\n\n  See all options:\n    beavy host check --help\n"
        );
        let plain = render(PLAIN, |h| h.error(&err).unwrap());
        assert!(plain.starts_with("Error: Unknown option --x for beavy host check.\n"));
        let colored = render(
            Presentation {
                color: true,
                ..RICH
            },
            |h| h.error(&err).unwrap(),
        );
        assert!(colored.contains("\x1b[31m✗\x1b[0m"));
        assert!(colored.contains("\x1b[36mbeavy host check --help\x1b[0m"));
    }

    #[test]
    fn warnings_and_success_use_glyphs_only_when_rich() {
        assert_eq!(render(RICH, |h| h.warning("w").unwrap()), "! w\n");
        assert_eq!(render(PLAIN, |h| h.warning("w").unwrap()), "Warning: w\n");
        assert_eq!(render(RICH, |h| h.success("Done x").unwrap()), "✓ Done x\n");
        assert_eq!(render(PLAIN, |h| h.success("Done x").unwrap()), "Done x\n");
    }

    #[derive(Serialize)]
    struct Thing {
        thing: u32,
    }

    #[test]
    fn json_mode_keeps_stderr_silent_and_stdout_to_one_document() {
        let mode = resolve(json(), NO_TTY, &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        {
            let mut output = Output::new(mode, &mut out, &mut err);
            output.stderr().line("context").unwrap();
            output
                .warnings(&[Warning {
                    reason: crate::WarningReason::CleanupFailed,
                    message: "w".into(),
                }])
                .unwrap();
            output.json(&Document::new(Thing { thing: 1 })).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"schemaVersion\":1,\"thing\":1}\n"
        );
        assert!(err.is_empty());
    }

    #[test]
    fn json_is_indented_only_on_a_terminal() {
        let mode = resolve(json(), ALL_TTY, &[("FORCE_COLOR", "1")]);
        let mut out = Vec::new();
        Output::new(mode, &mut out, io::sink())
            .json(&Document::new(Thing { thing: 1 }))
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\n  \"schemaVersion\": 1,\n  \"thing\": 1\n}\n"
        );
    }

    #[test]
    fn json_errors_are_one_line_on_stderr() {
        let mode = resolve(json(), ALL_TTY, &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let failure = Error::new(Reason::NotFound, "Couldn't find directory ./x.");
        Output::new(mode, &mut out, &mut err)
            .error(&failure)
            .unwrap();
        assert!(out.is_empty());
        let err = String::from_utf8(err).unwrap();
        assert_eq!(err.lines().count(), 1);
        let doc: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(doc["reason"], "not_found");
    }
}
