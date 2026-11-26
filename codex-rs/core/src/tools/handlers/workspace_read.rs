use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::workspace_read_spec::WORKSPACE_READ_TOOL_NAME;
use crate::tools::handlers::workspace_read_spec::create_workspace_read_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const MAX_DECLARED_PATHS: usize = 16;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_QUERY_BYTES: usize = 256;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceReadAction {
    Read,
    Search,
    Compare,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceReadArgs {
    action: WorkspaceReadAction,
    paths: Vec<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

pub(crate) struct WorkspaceReadHandler;

impl ToolExecutor<ToolInvocation> for WorkspaceReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WORKSPACE_READ_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_workspace_read_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl WorkspaceReadHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        if !matches!(
            invocation.turn.session_source,
            SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec
        ) {
            return Err(FunctionCallError::RespondToModel(
                "workspace_read is available only to the authorized task parent".into(),
            ));
        }
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "workspace_read requires structured function arguments".into(),
            ));
        };
        let args: WorkspaceReadArgs = parse_arguments(arguments)?;
        let scope = invocation
            .session
            .active_task_scope()
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "workspace_read requires an active common-harness task".into(),
                )
            })?;
        let completion = invocation
            .session
            .rencrow_completion_state(&scope.root_task_id)
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "workspace_read cannot find the active host completion state".into(),
                )
            })?;
        let common = completion.common_harness.as_ref().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "workspace_read requires an active common-harness v1 task".into(),
            )
        })?;
        if !matches!(
            common.run_state.status,
            TaskRunStatusV1::Queued
                | TaskRunStatusV1::Running
                | TaskRunStatusV1::Verifying
                | TaskRunStatusV1::Waiting
                | TaskRunStatusV1::Blocked
        ) {
            return Err(FunctionCallError::RespondToModel(
                "workspace_read is unavailable after the common-harness task is terminal".into(),
            ));
        }

        let allowed_paths = declared_paths(&common.request)?;
        if allowed_paths.is_empty() || allowed_paths.len() > MAX_DECLARED_PATHS {
            return Err(FunctionCallError::RespondToModel(format!(
                "workspace_read requires 1 through {MAX_DECLARED_PATHS} host-declared paths"
            )));
        }
        let paths = validate_requested_paths(&args, &allowed_paths)?;
        let max_output_bytes = validate_output_limit(args.max_output_bytes)?;

        let workspace_root = invocation
            .turn
            .config
            .cwd
            .as_path()
            .canonicalize()
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "workspace_read cannot resolve the current workspace: {error}"
                ))
            })?;
        let binding =
            crate::common_harness_io::ArtifactBindingV1::for_canonical_root(&workspace_root)
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "workspace_read cannot bind workspace: {error}"
                    ))
                })?;
        if common.request.workspace_ref != binding.workspace_binding_ref {
            return Err(FunctionCallError::RespondToModel(
                "workspace_read current workspace does not match the host task binding".into(),
            ));
        }
        let before = crate::common_harness_io::ArtifactSetV1::capture_bound(
            &workspace_root,
            &paths,
            &binding,
        )
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace_read path validation failed: {error}"
            ))
        })?;
        let contents = paths
            .iter()
            .map(|path| read_bound_file(&workspace_root, path))
            .collect::<Result<Vec<_>, _>>()?;
        let after = crate::common_harness_io::ArtifactSetV1::capture_bound(
            &workspace_root,
            &paths,
            &binding,
        )
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace_read path recheck failed: {error}"
            ))
        })?;
        if before.digest != after.digest || before.entries != after.entries {
            return Err(FunctionCallError::RespondToModel(
                "workspace_read target changed while the read was in progress".into(),
            ));
        }
        let rendered = render_result(&args, &paths, &contents)?;
        let bounded = bound_output(rendered, max_output_bytes);
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            bounded,
            Some(true),
        )))
    }
}

pub(crate) async fn read_budget_identity(
    invocation: &ToolInvocation,
) -> Result<String, FunctionCallError> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "workspace_read requires structured function arguments".into(),
        ));
    };
    let args: WorkspaceReadArgs = parse_arguments(arguments)?;
    validate_output_limit(args.max_output_bytes)?;
    let scope = invocation
        .session
        .active_task_scope()
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "workspace_read requires an active common-harness task".into(),
            )
        })?;
    let completion = invocation
        .session
        .rencrow_completion_state(&scope.root_task_id)
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "workspace_read cannot find the active host completion state".into(),
            )
        })?;
    let common = completion.common_harness.as_ref().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "workspace_read requires an active common-harness v1 task".into(),
        )
    })?;
    if !matches!(
        common.run_state.status,
        TaskRunStatusV1::Queued
            | TaskRunStatusV1::Running
            | TaskRunStatusV1::Verifying
            | TaskRunStatusV1::Waiting
            | TaskRunStatusV1::Blocked
    ) {
        return Err(FunctionCallError::RespondToModel(
            "workspace_read is unavailable after the common-harness task is terminal".into(),
        ));
    }
    let allowed = declared_paths(&common.request)?;
    let paths = validate_requested_paths(&args, &allowed)?;
    let workspace_root = invocation
        .turn
        .config
        .cwd
        .as_path()
        .canonicalize()
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace_read cannot resolve the current workspace: {error}"
            ))
        })?;
    let binding = crate::common_harness_io::ArtifactBindingV1::for_canonical_root(&workspace_root)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace_read cannot bind workspace: {error}"
            ))
        })?;
    if common.request.workspace_ref != binding.workspace_binding_ref {
        return Err(FunctionCallError::RespondToModel(
            "workspace_read current workspace does not match the host task binding".into(),
        ));
    }
    let action = match args.action {
        WorkspaceReadAction::Read => "read",
        WorkspaceReadAction::Search => "search",
        WorkspaceReadAction::Compare => "compare",
    };
    let query = args.query.as_deref().unwrap_or_default();
    Ok(format!(
        "{:?}:workspace_read:{action}:{paths:?}:{query}:{}",
        invocation.turn.config.cwd, args.case_sensitive
    ))
}

impl CoreToolRuntime for WorkspaceReadHandler {}

fn declared_paths(
    request: &codex_history::task_context::TaskRequestV1,
) -> Result<BTreeSet<String>, FunctionCallError> {
    let paths = request
        .conditions
        .iter()
        .flat_map(|condition| {
            condition.dependency_refs.iter().chain(
                condition
                    .check
                    .iter()
                    .flat_map(|check| check.cwd_binding_refs.iter()),
            )
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if paths.len() > MAX_DECLARED_PATHS {
        return Err(FunctionCallError::RespondToModel(format!(
            "active task declares more than {MAX_DECLARED_PATHS} workspace paths"
        )));
    }
    Ok(paths)
}

fn validate_output_limit(value: Option<usize>) -> Result<usize, FunctionCallError> {
    let limit = value.unwrap_or(MAX_OUTPUT_BYTES);
    if !(1..=MAX_OUTPUT_BYTES).contains(&limit) {
        return Err(FunctionCallError::RespondToModel(format!(
            "max_output_bytes must be between 1 and {MAX_OUTPUT_BYTES}"
        )));
    }
    Ok(limit)
}

fn validate_requested_paths(
    args: &WorkspaceReadArgs,
    allowed: &BTreeSet<String>,
) -> Result<Vec<PathBuf>, FunctionCallError> {
    let required_count = match args.action {
        WorkspaceReadAction::Read | WorkspaceReadAction::Search => 1,
        WorkspaceReadAction::Compare => 2,
    };
    if args.paths.len() != required_count {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace_read action requires exactly {required_count} path(s)"
        )));
    }
    let mut paths = Vec::with_capacity(args.paths.len());
    for raw in &args.paths {
        let path = Path::new(raw);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || !allowed.contains(raw)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "workspace_read path `{raw}` is not a declared workspace-relative task path"
            )));
        }
        paths.push(path.to_path_buf());
    }
    match args.action {
        WorkspaceReadAction::Read => {}
        WorkspaceReadAction::Search => {
            let query = args.query.as_deref().unwrap_or_default();
            if query.is_empty() || query.len() > MAX_QUERY_BYTES {
                return Err(FunctionCallError::RespondToModel(format!(
                    "search query must contain 1 through {MAX_QUERY_BYTES} UTF-8 bytes"
                )));
            }
            if args
                .max_results
                .is_some_and(|limit| !(1..=MAX_SEARCH_RESULTS).contains(&limit))
            {
                return Err(FunctionCallError::RespondToModel(format!(
                    "max_results must be between 1 and {MAX_SEARCH_RESULTS}"
                )));
            }
        }
        WorkspaceReadAction::Compare => {}
    }
    Ok(paths)
}

fn read_bound_file(root: &Path, relative: &Path) -> Result<String, FunctionCallError> {
    let path = root.join(relative);
    let canonical = path.canonicalize().map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "workspace_read cannot resolve `{}`: {error}",
            relative.display()
        ))
    })?;
    if !canonical.starts_with(root) {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace_read path `{}` resolves outside the workspace",
            relative.display()
        )));
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "workspace_read cannot inspect `{}`: {error}",
            relative.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() as usize > MAX_FILE_BYTES
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace_read requires a regular non-symlink file no larger than {MAX_FILE_BYTES} bytes"
        )));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&canonical).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "workspace_read cannot open `{}`: {error}",
            relative.display()
        ))
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!("workspace_read failed: {error}"))
        })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace_read file exceeds {MAX_FILE_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "workspace_read requires UTF-8 text for `{}`: {error}",
            relative.display()
        ))
    })
}

fn render_result(
    args: &WorkspaceReadArgs,
    paths: &[PathBuf],
    contents: &[String],
) -> Result<String, FunctionCallError> {
    match args.action {
        WorkspaceReadAction::Read => Ok(format!("{}\n{}", paths[0].display(), contents[0])),
        WorkspaceReadAction::Search => {
            let query = args.query.as_deref().unwrap_or_default();
            let query = if args.case_sensitive {
                query.to_string()
            } else {
                query.to_lowercase()
            };
            let max_results = args.max_results.unwrap_or(MAX_SEARCH_RESULTS);
            let mut matches = Vec::new();
            for (line_no, line) in contents[0].lines().enumerate() {
                let haystack = if args.case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                if haystack.contains(&query) {
                    matches.push(format!("{}:{}:{}", paths[0].display(), line_no + 1, line));
                    if matches.len() >= max_results {
                        break;
                    }
                }
            }
            Ok(if matches.is_empty() {
                "No matching lines.".into()
            } else {
                matches.join("\n")
            })
        }
        WorkspaceReadAction::Compare => Ok(format!(
            "{} and {} are {} (sha256 {} vs {}).",
            paths[0].display(),
            paths[1].display(),
            if contents[0] == contents[1] {
                "identical"
            } else {
                "different"
            },
            sha256(contents[0].as_bytes()),
            sha256(contents[1].as_bytes()),
        )),
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "workspace_read_tests.rs"]
mod tests;

fn bound_output(mut output: String, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output;
    }
    let marker = "\n[output truncated by host]";
    if max_bytes <= marker.len() {
        return marker[..max_bytes].to_string();
    }
    let limit = max_bytes.saturating_sub(marker.len());
    let mut boundary = limit.min(output.len());
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.truncate(boundary);
    output.push_str(marker);
    output
}
