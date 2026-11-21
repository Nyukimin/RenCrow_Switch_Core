//! Conservative identity for direct native check commands.
use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckIdentity {
    pub(crate) cwd: PathBuf,
    pub(crate) cwd_reliable: bool,
    pub(crate) runner: Vec<String>,
    pub(crate) environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedCheck {
    pub(crate) identity: CheckIdentity,
}

/// Parses only literal setup clauses followed by one direct runner command.
pub(crate) fn parse(
    command: &str,
    base_cwd: &Path,
    is_supported_runner: impl FnOnce(&[String]) -> bool,
) -> Option<ParsedCheck> {
    if command.contains(['\n', '\r']) {
        return None;
    }
    let command = command.strip_suffix(" 2>&1").unwrap_or(command);
    let parts = split_and(command)?;
    let mut cwd = base_cwd.to_path_buf();
    let mut cwd_reliable = !has_parent_component(base_cwd);
    let mut environment = BTreeMap::new();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        let words = shlex::split(part)?;
        match words.as_slice() {
            [cd, path] if cd == "cd" => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    cwd = path;
                    cwd_reliable = !has_parent_component(&cwd);
                } else {
                    cwd = cwd.join(path);
                    cwd_reliable = false;
                }
            }
            [export, assignments @ ..] if export == "export" && !assignments.is_empty() => {
                for assignment in assignments {
                    let (key, value) = assignment.split_once('=')?;
                    if !valid_env_key(key) {
                        return None;
                    }
                    environment.insert(key.into(), value.into());
                }
            }
            _ => return None,
        }
    }

    let mut runner = shlex::split(parts.last()?)?;
    let mut index = 0;
    while let Some((key, value)) = runner.get(index).and_then(|word| word.split_once('=')) {
        if !valid_env_key(key) {
            return None;
        }
        environment.insert(key.into(), value.into());
        index += 1;
    }
    if index > 0 {
        runner.drain(..index);
    }
    if runner.is_empty() || !is_supported_runner(&runner) {
        return None;
    }

    Some(ParsedCheck {
        identity: CheckIdentity {
            cwd: normalize_cwd(&cwd),
            cwd_reliable,
            runner,
            environment,
        },
    })
}

pub(crate) fn is_cargo_not_found_startup_failure(
    identity: &CheckIdentity,
    exit_code: i64,
    output: &str,
    output_may_be_truncated: bool,
) -> bool {
    if exit_code != 127 || output_may_be_truncated || !identity.environment.is_empty() {
        return false;
    }
    let mut lines = output.lines();
    let exact_command_line = lines.next()
        == Some(
            "RUST_MIN_STACK=16777216 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast \"$@\"",
        );
    let missing_cargo_line = lines.next() == Some("test: cargo: command not found");
    let recipe_failure = lines.next().is_some_and(|line| {
        line.strip_prefix("error: recipe `test` failed on line ")
            .and_then(|line| line.strip_suffix(" with exit code 127"))
            .is_some_and(|line| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
    });
    let only_expected_lines = lines.next().is_none();
    exact_command_line
        && missing_cargo_line
        && recipe_failure
        && only_expected_lines
        && identity
            .runner
            .first()
            .is_some_and(|program| executable_name(program) == "just")
        && identity
            .runner
            .get(1)
            .is_some_and(|subcommand| subcommand == "test")
}

/// Allows only the observed repair: inherited PATH/toolchain resolution failure, then
/// the same `just test` argv with explicit PATH and a newly pinned toolchain.
pub(crate) fn is_cargo_not_found_retry(previous: &CheckIdentity, current: &CheckIdentity) -> bool {
    if !previous.cwd_reliable
        || !current.cwd_reliable
        || previous.cwd != current.cwd
        || previous.runner != current.runner
        || !previous.environment.is_empty()
    {
        return false;
    }
    let Some(path) = current.environment.get("PATH") else {
        return false;
    };
    if path.is_empty()
        || current
            .environment
            .keys()
            .any(|key| !matches!(key.as_str(), "PATH" | "RUSTUP_TOOLCHAIN"))
    {
        return false;
    }
    if current
        .environment
        .get("RUSTUP_TOOLCHAIN")
        .is_some_and(String::is_empty)
    {
        return false;
    }
    current
        .runner
        .first()
        .is_some_and(|program| executable_name(program) == "just")
        && current
            .runner
            .get(1)
            .is_some_and(|subcommand| subcommand == "test")
}

fn executable_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

fn valid_env_key(key: &str) -> bool {
    key.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn normalize_cwd(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn split_and(command: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                '$' | '`' => return None,
                _ => {}
            },
            _ => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                '$' | '`' | ';' | '|' | '>' | '<' | '(' | ')' | '{' | '}' | '*' | '?' | '['
                | '~' | '#' => return None,
                '&' if chars.peek().is_some_and(|(_, next)| *next == '&') => {
                    chars.next();
                    parts.push(&command[start..index]);
                    start = index + 2;
                }
                '&' => return None,
                _ => {}
            },
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    parts.push(&command[start..]);
    Some(parts)
}

#[cfg(test)]
#[path = "rencrow_check_identity_tests.rs"]
mod tests;
