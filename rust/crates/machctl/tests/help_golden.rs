//! Byte-compare `machctl --help` against the Python `argparse` golden corpus.
//!
//! The Rust port keeps `clap` for parsing but renders help with an `argparse`
//! formatter port. These files pin that rendering: if `clap` metadata changes
//! shape, or the formatter drifts from CPython's `HelpFormatter`/`textwrap`,
//! the byte comparison fails instead of silently reformatting the CLI.

use std::path::Path;

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

const SUBCOMMANDS: &[&str] = &[
    "environment",
    "acquire",
    "wait",
    "list",
    "images",
    "images-show",
    "status",
    "exec",
    "push",
    "pull",
    "heartbeat",
    "release",
    "capabilities",
    "acquisition-capabilities",
    "console-resolve",
    "console-open",
    "console-cancel",
    "gc",
];

/// Widths with a recorded corpus. The narrow values cross `argparse`'s
/// minimum help position, and the wide values go past where wrapping stops.
const WIDTHS: &[&str] = &[
    "20", "24", "25", "30", "33", "40", "80", "120", "160", "240", "300",
];

/// AC-35: `machctl --help` and every subcommand's help match the golden files.
#[test]
fn ac35_help_matches_argparse_golden_corpus() {
    for width in WIDTHS {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/help")
            .join(width);

        check(&directory, "top", &["--help"], width);
        for name in SUBCOMMANDS {
            check(&directory, name, &[name, "--help"], width);
        }
    }
}

/// `COLUMNS` is read with Python's `int()`, so a value Python accepts has to
/// select that width and a value Python rejects has to fall back.
///
/// Every pair is `(COLUMNS, the width whose output it must equal)`. `shutil.
/// get_terminal_size` falls back to the tty and then to 80 when the value is
/// missing or unparseable; `stdout` is a pipe in this test, so the fallback is
/// 80.
#[test]
fn columns_parsing_matches_python_int() {
    let cases: [(&str, &str); 17] = [
        ("+80", "80"),
        (" 80", "80"),
        ("80 ", "80"),
        ("1_0", "10"),
        // U+0668 U+0660 and U+FF18 U+FF10: non-ASCII decimal digits for 80.
        ("\u{668}\u{660}", "80"),
        ("\u{ff18}\u{ff10}", "80"),
        ("800", "800"),
        ("", "80"),
        ("abc", "80"),
        // Zero and negative widths fall through to the terminal.
        ("0", "80"),
        ("-5", "80"),
        ("1__0", "80"),
        ("_80", "80"),
        ("80_", "80"),
        ("0x50", "80"),
        ("8.0", "80"),
        ("1e2", "80"),
    ];
    for (columns, equivalent) in cases {
        assert_eq!(
            capture(&["--help"], columns),
            capture(&["--help"], equivalent),
            "COLUMNS={columns:?} should render like COLUMNS={equivalent:?}"
        );
    }
}

/// With colour forced on through a pipe, the renderer must only insert escape
/// codes: stripping ANSI from the coloured output has to reproduce the plain
/// golden byte for byte. This exercises the real `can_colorize` gate without a
/// pty, because a non-empty `FORCE_COLOR` turns colour on for a non-tty.
#[test]
fn forced_color_help_matches_plain_golden_after_decoloring() {
    for width in WIDTHS {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/help")
            .join(width);

        check_colored(&directory, "top", &["--help"], width);
        for name in SUBCOMMANDS {
            check_colored(&directory, name, &[name, "--help"], width);
        }
    }
}

fn check_colored(directory: &Path, name: &str, args: &[&str], columns: &str) {
    let expected = std::fs::read(directory.join(format!("{name}.txt")))
        .unwrap_or_else(|error| panic!("read {name}.txt: {error}"));

    let output = service_layout::test_build::harness::command(MACHCTL)
        .args(args)
        .env("COLUMNS", columns)
        // The fixture tests FORCE_COLOR, whose documented precedence is
        // below NO_COLOR and PYTHON_COLORS; isolate those inherited inputs.
        .env_remove("NO_COLOR")
        .env_remove("PYTHON_COLORS")
        .env("FORCE_COLOR", "1")
        .output()
        .unwrap_or_else(|error| panic!("spawn machctl {args:?}: {error}"));

    assert!(
        output.status.success(),
        "`machctl {args:?}` exited with {:?}:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.windows(2).any(|window| window == b"\x1b["),
        "FORCE_COLOR did not colour `machctl {args:?}` at COLUMNS={columns}"
    );
    assert_eq!(
        strip_ansi(&output.stdout),
        expected,
        "colour changed the visible text of `machctl {args:?}` at COLUMNS={columns}"
    );
}

/// Remove ANSI SGR sequences so coloured output can be compared to plain text.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'm' {
                index += 1;
            }
            index += 1;
        } else {
            stripped.push(bytes[index]);
            index += 1;
        }
    }
    stripped
}

fn capture(args: &[&str], columns: &str) -> Vec<u8> {
    let output = service_layout::test_build::harness::command(MACHCTL)
        .args(args)
        .env("COLUMNS", columns)
        // Pin plain output so an inherited `FORCE_COLOR` cannot colour the
        // golden comparison.
        .env("PYTHON_COLORS", "0")
        .output()
        .unwrap_or_else(|error| panic!("spawn machctl {args:?}: {error}"));
    assert!(
        output.status.success(),
        "`machctl {args:?}` at COLUMNS={columns:?} exited with {:?}:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn check(directory: &Path, name: &str, args: &[&str], columns: &str) {
    let expected = std::fs::read(directory.join(format!("{name}.txt")))
        .unwrap_or_else(|error| panic!("read {name}.txt: {error}"));

    let output = service_layout::test_build::harness::command(MACHCTL)
        .args(args)
        .env("COLUMNS", columns)
        // Pin plain output so an inherited `FORCE_COLOR` cannot colour the
        // golden comparison.
        .env("PYTHON_COLORS", "0")
        .output()
        .unwrap_or_else(|error| panic!("spawn machctl {args:?}: {error}"));

    assert!(
        output.status.success(),
        "`machctl {args:?}` exited with {:?}:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    if output.stdout != expected {
        panic!(
            "`machctl {args:?}` at COLUMNS={columns} differs from {name}.txt:\n{}",
            diff(&expected, &output.stdout)
        );
    }
}

fn diff(expected: &[u8], actual: &[u8]) -> String {
    let expected = String::from_utf8_lossy(expected);
    let actual = String::from_utf8_lossy(actual);
    let mut report = String::new();
    for (index, (left, right)) in expected.lines().zip(actual.lines()).enumerate() {
        if left != right {
            report.push_str(&format!("  line {}:\n  - {left}\n  + {right}\n", index + 1));
        }
    }
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    if expected_lines.len() != actual_lines.len() {
        report.push_str(&format!(
            "  line count: golden {} vs actual {}\n",
            expected_lines.len(),
            actual_lines.len()
        ));
    }
    if report.is_empty() {
        report.push_str("  (bytes differ; see line count)\n");
    }
    report
}
