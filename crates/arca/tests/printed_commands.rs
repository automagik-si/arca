//! No command line the program or the installer prints holds a value in angle brackets. A person pastes such a line
//! into a shell, and a shell reads `<name>` as two redirections: one from a file called `name`, one into whatever
//! follows. A value the person fills in is a word in capitals instead, with a sentence that says what goes in its
//! place (`arca token revoke TOKEN_ID ..., with its id in place of TOKEN_ID`).
//!
//! What is not a command line, and keeps the brackets: the usage (`arca help`, and the usage a refusal shows), which
//! is a grammar with `[...]` and `|` and is never pasted; and a description of a value's form inside a refusal
//! (`https://<host>/<path>`, `<registry>/<name>@sha256:<64 hex digits>`).

use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The first value in angle brackets in `text`: `<`, a lower-case letter, then letters, digits, spaces and the marks a
/// placeholder holds, then `>`. A redirection (`</dev/null`, `<"$file"`) and a type (`Vec<u8>`, outside a string) are
/// not one.
fn placeholder(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(open) = text[at..].find('<').map(|found| found + at) {
        at = open + 1;
        if !bytes.get(at).is_some_and(u8::is_ascii_lowercase) {
            continue;
        }
        let rest = &text[at..];
        let Some(close) = rest.find('>') else {
            return None;
        };
        let inside = &rest[..close];
        if inside
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._/:'-".contains(c))
        {
            return Some(&text[open..=at + close]);
        }
    }
    None
}

/// Whether `line` tells a person a command: it names a program a person runs, or a flag followed by its value.
fn is_a_command(line: &str) -> bool {
    ["arca ", "ssh ", "install.sh", "systemctl ", "INCUS_SOCKET="]
        .iter()
        .any(|program| line.contains(program))
        || line.match_indices("--").any(|(at, _)| {
            line[at..]
                .split(' ')
                .nth(1)
                .is_some_and(|value| value.starts_with('<'))
        })
}

/// The part of a Rust source line that is inside a string literal, as far as one line can tell.
fn in_strings(line: &str) -> String {
    let mut out = String::new();
    let (mut inside, mut escaped) = (false, false);
    for c in line.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if inside => escaped = true,
            '"' => {
                inside = !inside;
                out.push(' ');
                continue;
            }
            _ => {}
        }
        if inside {
            out.push(c);
        }
    }
    out
}

#[test]
fn the_check_itself_tells_a_placeholder_from_a_redirection_and_a_type() {
    assert_eq!(
        placeholder("arca token revoke <id> --config x"),
        Some("<id>")
    );
    assert_eq!(
        placeholder("ssh -L 1:2:3 <this machine>, then"),
        Some("<this machine>")
    );
    assert_eq!(
        placeholder("--service-cidr <a.b.c.0/24>"),
        Some("<a.b.c.0/24>")
    );
    assert_eq!(placeholder("arca token revoke TOKEN_ID --config x"), None);
    assert_eq!(
        placeholder("status=$(\"$bin\" call x 2>/dev/null </dev/null)"),
        None
    );
    assert_eq!(placeholder("read -r first <\"$unit_file\" || true"), None);
    assert_eq!(
        in_strings("let x: Vec<u8> = f(\"arca call <verb>\");"),
        " arca call <verb> "
    );
    assert!(is_a_command("name it with --service-cidr <a.b.c.0/24>"));
    assert!(!is_a_command("expected https://<host>/<path>"));
}

/// The program's own words. Every source file of the crates that print for a person is read up to its tests; the
/// usage is left out, and so is a line that shows a command's usage.
#[test]
fn no_command_line_the_program_prints_holds_a_value_in_angle_brackets() {
    let mut found = Vec::new();
    let mut files = 0;
    for krate in ["arca", "arca-approval", "arca-mcp", "arca-api"] {
        let dir = repo().join("crates").join(krate).join("src");
        for entry in fs::read_dir(&dir).unwrap_or_else(|_| panic!("{} is read", dir.display())) {
            let path = entry.expect("an entry").path();
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            files += 1;
            let text = fs::read_to_string(&path).expect("a source file");
            let mut in_usage = false;
            for (number, line) in text.lines().enumerate() {
                if line.starts_with("#[cfg(test)]") {
                    break;
                }
                // The usage is one constant, from its name to the line that closes its text.
                if line.starts_with("const USAGE") {
                    in_usage = true;
                }
                if in_usage {
                    in_usage = !line.trim_end().ends_with("\";");
                    continue;
                }
                if line.trim_start().starts_with("//") || line.contains("usage:") {
                    continue;
                }
                let said = in_strings(line);
                if is_a_command(&said)
                    && let Some(value) = placeholder(&said)
                {
                    found.push(format!("{}:{}: {value}", path.display(), number + 1));
                }
            }
        }
    }
    assert!(files > 40, "the source files were found ({files})");
    assert!(
        found.is_empty(),
        "a printed command line holds a value in angle brackets; use a word in capitals and say what goes in its place:\n{}",
        found.join("\n")
    );
}

/// The installer's words: every line of the script that is not a comment.
#[test]
fn no_line_the_installer_prints_holds_a_value_in_angle_brackets() {
    let script = fs::read_to_string(repo().join("scripts/install.sh")).expect("the installer");
    let found: Vec<String> = script
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
        .filter_map(|(number, line)| {
            placeholder(line).map(|value| format!("{}: {value}", number + 1))
        })
        .collect();
    assert!(script.lines().count() > 1000, "the installer was read");
    assert!(
        found.is_empty(),
        "the installer prints a value in angle brackets; use a word in capitals and say what goes in its place:\n{}",
        found.join("\n")
    );
}
