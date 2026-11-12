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
        let close = rest.find('>')?;
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

/// Every string literal of a Rust source text, whole, with the line it starts on. A literal that runs over several
/// lines is one literal: a `\` at a line's end joins the next line to it, as the compiler does. Comments, character
/// literals (`'"'` among them) and lifetimes are passed over, so a quote in one of them opens no string.
fn string_literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let at = |index: usize| chars.get(index).copied();
    let mut found = Vec::new();
    let (mut line, mut i) = (1, 0);
    while i < chars.len() {
        match chars[i] {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if at(i + 1) == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if at(i + 1) == Some('*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && at(i + 1) == Some('/')) {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 2;
            }
            // A character literal with an escape: the quote, the backslash, the escaped character, up to its end.
            '\'' if at(i + 1) == Some('\\') => {
                i += 3;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            // A plain character literal; anything else that starts with a quote is a lifetime.
            '\'' if at(i + 2) == Some('\'') => i += 3,
            // A raw string: r"..." or r#"..."#, where nothing is an escape.
            'r' if (at(i + 1) == Some('"') || at(i + 1) == Some('#'))
                && !(i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) =>
            {
                let mut hashes = 0;
                while at(i + 1 + hashes) == Some('#') {
                    hashes += 1;
                }
                if at(i + 1 + hashes) != Some('"') {
                    i += 1;
                    continue;
                }
                let start = line;
                let mut text = String::new();
                i += 2 + hashes;
                while i < chars.len() {
                    if chars[i] == '"' && (0..hashes).all(|hash| at(i + 1 + hash) == Some('#')) {
                        break;
                    }
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    text.push(chars[i]);
                    i += 1;
                }
                i += 1 + hashes;
                found.push((start, text));
            }
            '"' => {
                let start = line;
                let mut text = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] != '\\' {
                        if chars[i] == '\n' {
                            line += 1;
                        }
                        text.push(chars[i]);
                        i += 1;
                        continue;
                    }
                    match at(i + 1) {
                        // The line goes on: the newline and the next line's indentation are not part of the text.
                        Some('\n') => {
                            i += 2;
                            line += 1;
                            while at(i).is_some_and(|c| c == ' ' || c == '\t') {
                                i += 1;
                            }
                        }
                        Some('n') => {
                            text.push('\n');
                            i += 2;
                        }
                        Some(escaped) => {
                            text.push(escaped);
                            i += 2;
                        }
                        None => i += 1,
                    }
                }
                i += 1;
                found.push((start, text));
            }
            _ => i += 1,
        }
    }
    found
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
    assert!(is_a_command("name it with --service-cidr <a.b.c.0/24>"));
    assert!(!is_a_command("expected https://<host>/<path>"));
}

/// The reading of the source: a string is one string however many lines it takes, and a quote that is not a string's
/// (in a comment, in a character, in a type) opens none. Before this, a string's second line was read as if it were
/// outside the string, and a placeholder on it was not seen.
#[test]
fn a_string_is_read_whole_across_its_lines() {
    let source = concat!(
        "let x: Vec<u8> = f(\"arca call <verb>\"); // a \"comment\" with <this>\n",
        "let quote = '\"'; let tick = '\\''; fn f<'a>(v: &'a str) {}\n",
        "refused(format!(\n",
        "    \"template {name}: its script is not installed. \\\n",
        "     `arca templates install --config <file>` puts it there\"\n",
        "));\n",
        "/* a block \"comment\" <too> */ let raw = r#\"arca link join <link> \"quoted\"\"#;\n",
        "let last = \"after\";\n",
    );
    assert_eq!(
        string_literals(source),
        [
            (1, "arca call <verb>".to_string()),
            (
                4,
                "template {name}: its script is not installed. `arca templates install --config <file>` puts it there"
                    .to_string()
            ),
            (7, "arca link join <link> \"quoted\"".to_string()),
            (8, "after".to_string()),
        ]
    );
    // The second line of the string that takes two is where its placeholder is.
    let literals = string_literals(source);
    let found: Vec<&str> = literals
        .iter()
        .filter(|(_, text)| is_a_command(text))
        .filter_map(|(_, text)| placeholder(text))
        .collect();
    assert_eq!(found, ["<verb>", "<file>", "<link>"]);
}

/// The program's own words. Every source file of the crates that print for a person is read up to its tests, one
/// whole string at a time; the usage is left out, and so is a string that shows a command's usage.
#[test]
fn no_command_line_the_program_prints_holds_a_value_in_angle_brackets() {
    let mut found = Vec::new();
    let (mut files, mut strings) = (0, 0);
    for krate in ["arca", "arca-approval", "arca-mcp", "arca-api"] {
        let dir = repo().join("crates").join(krate).join("src");
        for entry in fs::read_dir(&dir).unwrap_or_else(|_| panic!("{} is read", dir.display())) {
            let path = entry.expect("an entry").path();
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            files += 1;
            let text = fs::read_to_string(&path).expect("a source file");
            // The file's own tests quote what the program used to say, and are not what it says.
            let own = text
                .find("\n#[cfg(test)]")
                .map_or(text.as_str(), |tests| &text[..tests]);
            for (line, said) in string_literals(own) {
                strings += 1;
                if said.contains("usage:") {
                    continue;
                }
                if is_a_command(&said)
                    && let Some(value) = placeholder(&said)
                {
                    found.push(format!("{}:{line}: {value}", path.display()));
                }
            }
        }
    }
    assert!(
        files > 40 && strings > 5000,
        "the source was read ({files} files, {strings} strings)"
    );
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
