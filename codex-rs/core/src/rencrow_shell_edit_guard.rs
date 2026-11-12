//! Bounded preflight detection for clear shell-based file edits.
//!
//! The existing shell parser splits recognized simple commands. This module
//! checks only explicit copy/move commands, inline Python write APIs, and file
//! output redirects; it is not a general shell parser.
use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::parse_command::parse_shell_script;
use std::path::Path;

pub(crate) const EDIT_METHOD_REQUIRED: &str = "RENCROW_EDIT_METHOD_REQUIRED: shell file edit was not run; use apply_patch for the source edit, then run the required direct check.";

pub(crate) fn is_obvious_shell_edit(script: &str) -> bool {
    python_heredoc_writes_file(script)
        || parse_shell_script(script).iter().any(|command| {
            let command = match command {
                ParsedCommand::Read { cmd, .. }
                | ParsedCommand::ListFiles { cmd, .. }
                | ParsedCommand::Search { cmd, .. }
                | ParsedCommand::Unknown { cmd } => cmd,
            };
            is_obvious_command(command)
        })
}

fn is_obvious_command(command: &str) -> bool {
    if python_heredoc_writes_file(command) {
        return true;
    }
    let first_line = command.lines().next().unwrap_or_default();
    if has_file_output_redirect(first_line) {
        return true;
    }
    let words = shlex::split(command);
    let Some(words) = words else {
        return false;
    };

    words
        .split(|word| matches!(word.as_str(), ";" | "&&" | "||" | "|" | "&"))
        .any(is_obvious_command_segment)
}

fn is_obvious_command_segment(segment: &[String]) -> bool {
    let mut index = 0;
    while segment
        .get(index)
        .is_some_and(|word| is_environment_assignment(word))
    {
        index += 1;
    }
    if segment.get(index).is_some_and(|word| word == "env") {
        index += 1;
        while segment
            .get(index)
            .is_some_and(|word| is_environment_assignment(word))
        {
            index += 1;
        }
    }
    let Some(program) = segment.get(index) else {
        return false;
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let arguments = &segment[index + 1..];
    match program {
        "cp" | "mv" => !arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "--version" | "-h" | "-V")),
        "python" | "python3" => inline_python_writes_file(arguments),
        _ => false,
    }
}

fn is_environment_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    name.chars().next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == '_')
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

fn inline_python_writes_file(arguments: &[String]) -> bool {
    let Some(code_index) = arguments.iter().position(|argument| argument == "-c") else {
        return false;
    };
    let Some(source) = arguments.get(code_index + 1) else {
        return false;
    };
    python_source_writes_file(source)
}

fn python_heredoc_writes_file(command: &str) -> bool {
    let mut lines = command.lines();
    let Some(invocation) = lines.next() else {
        return false;
    };
    let Some((program, heredoc)) = invocation.split_once("<<") else {
        return false;
    };
    let Some(words) = shlex::split(program) else {
        return false;
    };
    let Some(program) = words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if !matches!(program, "python" | "python3") || !heredoc.trim().starts_with(['\'', '"']) {
        return false;
    }
    let delimiter = heredoc
        .trim()
        .trim_start_matches(['\'', '"'])
        .trim_end_matches(['\'', '"']);
    let body = lines
        .take_while(|line| line.trim() != delimiter)
        .collect::<Vec<_>>()
        .join("\n");
    python_source_writes_file(&body)
}

fn python_source_writes_file(source: &str) -> bool {
    let code = python_code_without_literals(source);
    if code.contains(".write_text(") || code.contains(".write_bytes(") {
        return true;
    }
    for (open_index, _) in code.match_indices("open(") {
        let arguments_start = open_index + "open(".len();
        let Some(relative_end) = code[arguments_start..].find(')') else {
            continue;
        };
        let arguments_end = arguments_start + relative_end;
        if open_mode_writes_file(&source[arguments_start..arguments_end]) {
            return true;
        }
    }
    false
}

fn open_mode_writes_file(arguments: &str) -> bool {
    let Some(mode) = arguments.split(',').nth(1) else {
        return false;
    };
    let mode = mode
        .trim()
        .strip_prefix("mode=")
        .unwrap_or(mode.trim())
        .trim();
    let Some(quote) = mode
        .chars()
        .next()
        .filter(|quote| matches!(quote, '\'' | '"'))
    else {
        return false;
    };
    let mode = mode.trim_matches(quote);
    matches!(mode.chars().next(), Some('w' | 'a' | 'x')) || mode.contains('+')
}

fn python_code_without_literals(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                bytes[index] = b' ';
                index += 1;
            }
            continue;
        }
        if !matches!(bytes[index], b'\'' | b'"') {
            index += 1;
            continue;
        }
        let quote = bytes[index];
        let triple = bytes.get(index..index + 3) == Some(&[quote, quote, quote]);
        let delimiter_len = if triple { 3 } else { 1 };
        for byte in &mut bytes[index..index + delimiter_len] {
            *byte = b' ';
        }
        index += delimiter_len;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                bytes[index] = b' ';
                index += 1;
                if index < bytes.len() {
                    if bytes[index] != b'\n' {
                        bytes[index] = b' ';
                    }
                    index += 1;
                }
            } else if triple && bytes.get(index..index + 3) == Some(&[quote, quote, quote]) {
                for byte in &mut bytes[index..index + 3] {
                    *byte = b' ';
                }
                index += 3;
                break;
            } else if !triple && bytes[index] == quote {
                bytes[index] = b' ';
                index += 1;
                break;
            } else {
                if bytes[index] != b'\n' {
                    bytes[index] = b' ';
                }
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn has_file_output_redirect(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') if character == '\'' => quote = None,
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            Some(_) => {}
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                '#' if index == 0
                    || command[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace) =>
                {
                    break;
                }
                '>' => {
                    if chars.peek().is_some_and(|(_, next)| *next == '>') {
                        chars.next();
                    }
                    if chars.peek().is_some_and(|(_, next)| *next == '&') {
                        chars.next();
                        let target = chars.peek().map_or(command.len(), |(next, _)| *next);
                        if command[target..]
                            .trim_start()
                            .chars()
                            .next()
                            .is_some_and(|target| target.is_ascii_digit())
                        {
                            continue;
                        }
                    }
                    let target = chars.peek().map_or(command.len(), |(next, _)| *next);
                    if is_dev_null_redirect_target(command, target) {
                        continue;
                    }
                    return true;
                }
                '&' if chars.peek().is_some_and(|(_, next)| *next == '>') => {
                    chars.next();
                    let target = chars.peek().map_or(command.len(), |(next, _)| *next);
                    if is_dev_null_redirect_target(command, target) {
                        continue;
                    }
                    return true;
                }
                _ => {}
            },
        }
    }
    false
}

fn is_dev_null_redirect_target(command: &str, start: usize) -> bool {
    let target = command[start..].trim_start();
    let Some(suffix) = target.strip_prefix("/dev/null") else {
        return false;
    };
    suffix.is_empty()
        || suffix.chars().next().is_some_and(|character| {
            character.is_ascii_whitespace()
                || matches!(character, ';' | '|' | '&' | '<' | '>' | ')')
        })
}

#[cfg(test)]
#[path = "rencrow_shell_edit_guard_tests.rs"]
mod tests;
