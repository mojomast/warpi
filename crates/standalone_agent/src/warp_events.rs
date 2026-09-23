//! Translation between the standalone bridge's neutral events and the Warp
//! multi-agent protobuf that the native controller/UI already understands.
//!
//! Nothing in this module is standalone-specific: it produces exactly the
//! `ResponseEvent` shapes the Warp server produces (same client actions, same
//! message envelopes, same finish reasons), which is why the native approval,
//! diff, tool-card, and persistence paths keep working unchanged.
//!
//! Provenance: the client-action shapes (`CreateTask` first, `AddMessagesToTask`
//! for the first text delta, `AppendToMessageContent` with the
//! `agent_output.text` field mask afterwards) were ported from OpenWarp's Go
//! adapter; see ../../../REUSE.md for the donor commit.

use std::collections::HashMap;

use prost_types::Timestamp;
use warp_multi_agent_api as api;

use crate::bridge::BridgeEvent;
use crate::protocol::ToolCallSpec;

/// Translate a model tool call (`workspace.*`) into a Warp tool call.
///
/// Unknown tool names and invalid arguments fail closed: the caller converts
/// the error into a tool-result error for the model and never executes a
/// partial action.
pub fn translate_tool_call(spec: &ToolCallSpec) -> Result<api::message::ToolCall, ToolTranslationError> {
    let args = &spec.arguments;
    let tool = match spec.name.as_str() {
        "workspace.shell" => {
            let command = string_arg(args, "command")?;
            let workdir = optional_string_arg(args, "workdir");
            let background = args
                .get("run_in_background")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let full_command = match workdir {
                Some(dir) if !dir.trim().is_empty() => format!("cd -- {} && {}", shell_quote(&dir), command),
                _ => command,
            };
            api::message::tool_call::Tool::RunShellCommand(api::message::tool_call::RunShellCommand {
                command: full_command,
                is_read_only: false,
                uses_pager: false,
                citations: Vec::new(),
                is_risky: false,
                wait_until_complete_value: Some(
                    api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(!background),
                ),
                risk_category: api::RiskCategory::NontrivialLocalChange as i32,
            })
        }
        "workspace.read_file" => {
            let path = string_arg(args, "path")?;
            let offset = integer_arg(args, "offset");
            let limit = integer_arg(args, "limit");
            let line_ranges = match (offset, limit) {
                (Some(start), Some(count)) => vec![api::FileContentLineRange {
                    start: start.max(1) as u32,
                    end: (start.max(1) + count.max(0)) as u32,
                }],
                _ => Vec::new(),
            };
            api::message::tool_call::Tool::ReadFiles(api::message::tool_call::ReadFiles {
                files: vec![api::message::tool_call::read_files::File { name: path, line_ranges }],
            })
        }
        "workspace.glob" => {
            let pattern = string_arg(args, "pattern")?;
            let search_dir = optional_string_arg(args, "path").unwrap_or_default();
            api::message::tool_call::Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
                patterns: vec![pattern],
                search_dir,
                max_matches: 200,
                max_depth: 0,
                min_depth: 0,
            })
        }
        "workspace.grep" => {
            let pattern = string_arg(args, "pattern")?;
            // The client-side Grep action does not expose case-insensitive or
            // glob filters; inline the case-insensitivity as a regex flag and
            // reject the glob filter rather than silently ignoring it.
            if args.get("glob").is_some_and(|value| !value.is_null()) {
                return Err(ToolTranslationError::UnsupportedArgument("glob".to_string()));
            }
            let ignore_case = args
                .get("ignore_case")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let query = if ignore_case { format!("(?i){pattern}") } else { pattern };
            api::message::tool_call::Tool::Grep(api::message::tool_call::Grep {
                queries: vec![query],
                path: optional_string_arg(args, "path").unwrap_or_default(),
            })
        }
        "workspace.write_file" => {
            let path = string_arg(args, "path")?;
            let content = string_arg(args, "content")?;
            api::message::tool_call::Tool::ApplyFileDiffs(api::message::tool_call::ApplyFileDiffs {
                summary: format!("create {path}"),
                diffs: Vec::new(),
                new_files: vec![api::message::tool_call::apply_file_diffs::NewFile {
                    file_path: path,
                    content,
                    allow_overwrite: true,
                }],
                deleted_files: Vec::new(),
                v4a_updates: Vec::new(),
            })
        }
        "workspace.edit_file" => {
            let path = string_arg(args, "path")?;
            let old_string = string_arg(args, "old_string")?;
            let new_string = string_arg(args, "new_string")?;
            if old_string == new_string {
                return Err(ToolTranslationError::NoOpEdit);
            }
            api::message::tool_call::Tool::ApplyFileDiffs(api::message::tool_call::ApplyFileDiffs {
                summary: format!("edit {path}"),
                diffs: vec![api::message::tool_call::apply_file_diffs::FileDiff {
                    file_path: path,
                    search: old_string,
                    replace: new_string,
                }],
                new_files: Vec::new(),
                deleted_files: Vec::new(),
                v4a_updates: Vec::new(),
            })
        }
        other => return Err(ToolTranslationError::UnknownTool(other.to_string())),
    };
    Ok(api::message::ToolCall { tool_call_id: spec.tool_call_id.clone(), tool: Some(tool) })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolTranslationError {
    #[error("tool {0} is not supported by the standalone workspace bridge")]
    UnknownTool(String),
    #[error("missing or invalid required argument: {0}")]
    InvalidArgument(String),
    #[error("argument {0} is not supported by the Warp executor")]
    UnsupportedArgument(String),
    #[error("edit arguments are identical; there is nothing to apply")]
    NoOpEdit,
}

fn string_arg(args: &serde_json::Value, name: &str) -> Result<String, ToolTranslationError> {
    args.get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolTranslationError::InvalidArgument(name.to_string()))
}

fn optional_string_arg(args: &serde_json::Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn integer_arg(args: &serde_json::Value, name: &str) -> Option<i64> {
    args.get(name).and_then(serde_json::Value::as_i64)
}

/// POSIX-shell-quote a path so `cd -- '<path>'` is safe for spaces, quotes,
/// Unicode, and shell metacharacters. Windows paths are handled by the
/// executor, which receives the raw `workdir` when it runs natively.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Result of rendering a Warp tool-call result for the model.
#[derive(Debug, Clone)]
pub struct RenderedToolResult {
    pub tool_call_id: String,
    pub status: crate::protocol::HelperToolResultStatus,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedStatus {
    Success,
    Rejected,
    Error,
}

/// Render a Warp `ToolCallResult` into bounded text for the model.
///
/// The rendering is deliberately defensive: unknown or missing payloads produce
/// an explicit "no representable result" string instead of pretending success.
pub fn render_tool_call_result(result: &api::message::ToolCallResult) -> (RenderedStatus, String) {
    let Some(payload) = result.result.as_ref() else {
        return (RenderedStatus::Error, "tool returned no result payload".to_string());
    };
    render_payload(Payload::from(payload))
}

/// Render the request-side tool result (same payloads, different envelope).
pub fn render_request_tool_call_result(
    result: &api::request::input::ToolCallResult,
) -> (RenderedStatus, String) {
    let Some(payload) = result.result.as_ref() else {
        return (RenderedStatus::Error, "tool returned no result payload".to_string());
    };
    render_payload(Payload::from(payload))
}

/// Borrowed view over the tool-result payloads the standalone bridge can render.
enum Payload<'a> {
    Shell(&'a api::RunShellCommandResult),
    Read(&'a api::ReadFilesResult),
    Diffs(&'a api::ApplyFileDiffsResult),
    Grep(&'a api::GrepResult),
    GlobV2(&'a api::FileGlobV2Result),
    Glob(&'a api::FileGlobResult),
    Unsupported,
}

impl<'a> From<&'a api::message::tool_call_result::Result> for Payload<'a> {
    fn from(value: &'a api::message::tool_call_result::Result) -> Self {
        use api::message::tool_call_result::Result as R;
        match value {
            R::RunShellCommand(shell) => Payload::Shell(shell),
            R::ReadFiles(read) => Payload::Read(read),
            R::ApplyFileDiffs(diffs) => Payload::Diffs(diffs),
            R::Grep(grep) => Payload::Grep(grep),
            R::FileGlobV2(glob) => Payload::GlobV2(glob),
            #[allow(deprecated)]
            R::FileGlob(glob) => Payload::Glob(glob),
            _ => Payload::Unsupported,
        }
    }
}

impl<'a> From<&'a api::request::input::tool_call_result::Result> for Payload<'a> {
    fn from(value: &'a api::request::input::tool_call_result::Result) -> Self {
        use api::request::input::tool_call_result::Result as R;
        match value {
            R::RunShellCommand(shell) => Payload::Shell(shell),
            R::ReadFiles(read) => Payload::Read(read),
            R::ApplyFileDiffs(diffs) => Payload::Diffs(diffs),
            R::Grep(grep) => Payload::Grep(grep),
            R::FileGlobV2(glob) => Payload::GlobV2(glob),
            #[allow(deprecated)]
            R::FileGlob(glob) => Payload::Glob(glob),
            _ => Payload::Unsupported,
        }
    }
}

fn render_payload(payload: Payload<'_>) -> (RenderedStatus, String) {
    match payload {
        Payload::Shell(shell) => render_shell_result(shell),
        Payload::Read(read) => render_read_result(read),
        Payload::Diffs(diffs) => render_diff_result(diffs),
        Payload::Grep(grep) => render_grep_result(grep),
        Payload::GlobV2(glob) => render_glob_result(glob),
        Payload::Glob(glob) => match glob.result.as_ref() {
            Some(api::file_glob_result::Result::Success(success)) => {
                (RenderedStatus::Success, bounded(&success.matched_files, 64 * 1024))
            }
            Some(api::file_glob_result::Result::Error(error)) => {
                (RenderedStatus::Error, bounded(&error.message, 8 * 1024))
            }
            None => (RenderedStatus::Error, "file glob returned no result".to_string()),
        },
        Payload::Unsupported => (
            RenderedStatus::Error,
            "tool result type is not supported by the standalone bridge".to_string(),
        ),
    }
}

fn render_shell_result(shell: &api::RunShellCommandResult) -> (RenderedStatus, String) {
    use api::run_shell_command_result::Result as R;
    match shell.result.as_ref() {
        Some(R::CommandFinished(finished)) => {
            let mut text = String::new();
            if !finished.output.is_empty() {
                text.push_str(&bounded(&finished.output, 256 * 1024));
            }
            if !text.ends_with('\n') && !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("exit code: {}", finished.exit_code));
            if finished.exit_code == 0 {
                (RenderedStatus::Success, text)
            } else {
                (RenderedStatus::Error, text)
            }
        }
        Some(R::PermissionDenied(denied)) => (
            RenderedStatus::Rejected,
            format!(
                "The user did not permit this command (reason: {:?}).",
                denied.reason.as_ref().map_or_else(|| "unspecified".to_string(), |reason| format!("{reason:?}"))
            ),
        ),
        Some(R::LongRunningCommandSnapshot(snapshot)) => (
            RenderedStatus::Success,
            format!(
                "Command is still running (command_id: {}).\n{}",
                snapshot.command_id,
                bounded(&snapshot.output, 64 * 1024)
            ),
        ),
        None => (RenderedStatus::Error, "shell command returned no result".to_string()),
    }
}

fn render_read_result(read: &api::ReadFilesResult) -> (RenderedStatus, String) {
    use api::read_files_result::Result as R;
    match read.result.as_ref() {
        Some(R::TextFilesSuccess(success)) => {
            let mut text = String::new();
            for file in &success.files {
                text.push_str(&format!("--- {} ---\n", file.file_path));
                text.push_str(&bounded(&file.content, 128 * 1024));
                text.push('\n');
            }
            for failed in &success.failed_reads {
                text.push_str(&format!("--- {} (failed) ---\n{}\n", failed.path, failed.message));
            }
            if text.is_empty() {
                return (RenderedStatus::Error, "no files were read".to_string());
            }
            (RenderedStatus::Success, bounded(&text, 512 * 1024))
        }
        Some(R::AnyFilesSuccess(success)) => {
            let mut text = String::new();
            for file in &success.files {
                let path = match file.content.as_ref() {
                    Some(api::any_file_content::Content::TextContent(text_content)) => text_content.file_path.clone(),
                    Some(api::any_file_content::Content::BinaryContent(binary)) => binary.file_path.clone(),
                    None => "unknown".to_string(),
                };
                text.push_str(&format!("--- {path} ---\n(binary or non-UTF8 content omitted)\n"));
            }
            for failed in &success.failed_reads {
                text.push_str(&format!("--- {} (failed) ---\n{}\n", failed.path, failed.message));
            }
            (RenderedStatus::Success, text)
        }
        Some(R::Error(error)) => (RenderedStatus::Error, error.message.clone()),
        None => (RenderedStatus::Error, "read returned no result".to_string()),
    }
}

fn render_diff_result(diffs: &api::ApplyFileDiffsResult) -> (RenderedStatus, String) {
    use api::apply_file_diffs_result::Result as R;
    match diffs.result.as_ref() {
        Some(R::Success(success)) => {
            let mut text = String::new();
            for file in &success.updated_files_v2 {
                if let Some(content) = file.file.as_ref() {
                    text.push_str(&format!("updated {}\n", content.file_path));
                }
            }
            #[allow(deprecated)]
            for file in &success.updated_files {
                text.push_str(&format!("updated {}\n", file.file_path));
            }
            for file in &success.deleted_files {
                text.push_str(&format!("deleted {}\n", file.file_path));
            }
            if text.is_empty() {
                text.push_str("no files were changed\n");
            }
            (RenderedStatus::Success, text)
        }
        Some(R::Error(error)) => (RenderedStatus::Error, error.message.clone()),
        None => (RenderedStatus::Error, "file edit returned no result".to_string()),
    }
}

fn render_grep_result(grep: &api::GrepResult) -> (RenderedStatus, String) {
    use api::grep_result::Result as R;
    match grep.result.as_ref() {
        Some(R::Success(success)) => {
            let mut text = String::new();
            for file in &success.matched_files {
                let lines: Vec<String> = file
                    .matched_lines
                    .iter()
                    .map(|line| line.line_number.to_string())
                    .collect();
                text.push_str(&format!("{}:{}\n", file.file_path, lines.join(",")));
            }
            if text.is_empty() {
                text.push_str("no matches\n");
            }
            (RenderedStatus::Success, bounded(&text, 64 * 1024))
        }
        Some(R::Error(error)) => (RenderedStatus::Error, error.message.clone()),
        None => (RenderedStatus::Error, "grep returned no result".to_string()),
    }
}

fn render_glob_result(glob: &api::FileGlobV2Result) -> (RenderedStatus, String) {
    use api::file_glob_v2_result::Result as R;
    match glob.result.as_ref() {
        Some(R::Success(success)) => {
            let mut text = success
                .matched_files
                .iter()
                .map(|file| file.file_path.clone())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                text.push_str("no files matched");
            }
            if !success.warnings.is_empty() {
                text.push_str(&format!("\nwarnings: {}", success.warnings));
            }
            (RenderedStatus::Success, bounded(&text, 64 * 1024))
        }
        Some(R::Error(error)) => (RenderedStatus::Error, error.message.clone()),
        None => (RenderedStatus::Error, "file glob returned no result".to_string()),
    }
}

fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated by standalone backend at {max_bytes} bytes]", &value[..end])
}

/// Pieces of a Warp request the standalone backend consumes.
#[derive(Debug, Clone, Default)]
pub struct RequestInputs {
    /// Root task id, used as the conversation identity.
    pub conversation_id: String,
    pub task_id: String,
    pub user_query: Option<String>,
    pub tool_results: Vec<RenderedToolResult>,
    pub working_dir: Option<String>,
    pub requested_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestInputError {
    /// Kept for compatibility; standalone mode now generates task ids for new
    /// conversations instead of failing.
    #[error("request has no task context; standalone mode cannot determine the conversation")]
    MissingTaskContext,
    #[error("request contains an unsupported input kind")]
    UnsupportedInput,
}

/// Extract the standalone-relevant inputs from a Warp multi-agent request.
///
/// A brand-new conversation has no server-backed task yet: the client sends an
/// empty task context and the (real) server generates the task id in its first
/// `CreateTask` action. Standalone mode behaves the same way, so an empty
/// `task_id` here means "generate one" rather than an error.
pub fn extract_request_inputs(request: &api::Request) -> Result<RequestInputs, RequestInputError> {
    let mut inputs = RequestInputs::default();
    if let Some(task_context) = request.task_context.as_ref()
        && let Some(root_task) = task_context
            .tasks
            .iter()
            .find(|task| {
                task.dependencies
                    .as_ref()
                    .is_none_or(|deps| deps.parent_task_id.is_empty())
            })
            .or_else(|| task_context.tasks.last())
    {
        inputs.conversation_id = root_task.id.clone();
        inputs.task_id = root_task.id.clone();
    }
    if let Some(input) = request.input.as_ref() {
        if let Some(context) = input.context.as_ref()
            && let Some(directory) = context.directory.as_ref()
            && !directory.pwd.is_empty()
        {
            inputs.working_dir = Some(directory.pwd.clone());
        }
        if let Some(api::request::input::Type::UserInputs(user_inputs)) = input.r#type.as_ref() {
            for entry in &user_inputs.inputs {
                match entry.input.as_ref() {
                    Some(api::request::input::user_inputs::user_input::Input::UserQuery(query)) => {
                        if let Some(existing) = inputs.user_query.as_mut() {
                            existing.push_str("\n\n");
                            existing.push_str(&query.query);
                        } else {
                            inputs.user_query = Some(query.query.clone());
                        }
                    }
                    Some(api::request::input::user_inputs::user_input::Input::ToolCallResult(result)) => {
                        let (status, text) = render_request_tool_call_result(result);
                        inputs.tool_results.push(RenderedToolResult {
                            tool_call_id: result.tool_call_id.clone(),
                            status: match status {
                                RenderedStatus::Success => crate::protocol::HelperToolResultStatus::Success,
                                RenderedStatus::Rejected => crate::protocol::HelperToolResultStatus::Rejected,
                                RenderedStatus::Error => crate::protocol::HelperToolResultStatus::Error,
                            },
                            text,
                        });
                    }
                    Some(_) => return Err(RequestInputError::UnsupportedInput),
                    None => {}
                }
            }
        }
    }
    Ok(inputs)
}

/// Builds one exchange's worth of `ResponseEvent`s from bridge events.
///
/// One writer per Warp request. It remembers which assistant messages it has
/// already created so text deltas turn into `AppendToMessageContent` updates of
/// an existing message rather than duplicated messages.
pub struct ExchangeWriter {
    pub task_id: String,
    conversation_id: String,
    request_id: String,
    run_id: String,
    create_task_sent: bool,
    /// Message ids for which `AddMessagesToTask` has been emitted.
    created_messages: HashMap<String, bool>,
    /// Message ids that have received at least one text delta.
    streamed_messages: HashMap<String, ()>,
    fallback_message_counter: u64,
}

impl ExchangeWriter {
    pub fn new(task_id: String, inputs: &RequestInputs, request_id: String, run_id: String) -> Self {
        Self {
            task_id,
            conversation_id: inputs.conversation_id.clone(),
            request_id,
            run_id,
            create_task_sent: false,
            created_messages: HashMap::new(),
            streamed_messages: HashMap::new(),
            fallback_message_counter: 0,
        }
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn init_event(&self) -> api::ResponseEvent {
        api::ResponseEvent {
            r#type: Some(api::response_event::Type::Init(api::response_event::StreamInit {
                conversation_id: self.conversation_id.clone(),
                request_id: self.request_id.clone(),
                run_id: self.run_id.clone(),
            })),
        }
    }

    /// Convert one bridge event. Terminal events (pause/settle/fail) close the
    /// exchange and must be the last events written.
    pub fn write(&mut self, event: &BridgeEvent) -> Vec<api::ResponseEvent> {
        match event {
            BridgeEvent::Init { .. } => vec![self.init_event()],
            BridgeEvent::CreateTask { task_id } => {
                if self.create_task_sent {
                    return Vec::new();
                }
                self.create_task_sent = true;
                vec![client_actions(vec![api::client_action::Action::CreateTask(
                    api::client_action::CreateTask {
                        task: Some(api::Task { id: task_id.clone(), ..Default::default() }),
                    },
                )])]
            }
            BridgeEvent::TextDelta { message_id, delta } => self.write_text_delta(message_id, delta),
            BridgeEvent::TextMessage { message_id, text } => {
                if self.created_messages.contains_key(message_id) {
                    // Streaming already delivered this text; nothing to add.
                    return Vec::new();
                }
                self.write_text_delta(message_id, text)
            }
            BridgeEvent::ToolCalls { calls } => {
                let messages = calls
                    .iter()
                    .filter_map(|call| match translate_tool_call(call) {
                        Ok(tool_call) => Some(message_with_tool_call(&self.task_id, call.tool_call_id.clone(), tool_call)),
                        Err(error) => Some(message_with_tool_call(
                            &self.task_id,
                            call.tool_call_id.clone(),
                            api::message::ToolCall {
                                tool_call_id: call.tool_call_id.clone(),
                                tool: Some(api::message::tool_call::Tool::Server(
                                    api::message::tool_call::Server {
                                        payload: format!("unsupported_tool_call: {error}"),
                                    },
                                )),
                            },
                        )),
                    })
                    .collect::<Vec<_>>();
                vec![client_actions(vec![api::client_action::Action::AddMessagesToTask(
                    api::client_action::AddMessagesToTask {
                        task_id: self.task_id.clone(),
                        messages,
                    },
                )])]
            }
            BridgeEvent::ExchangePaused { .. } => vec![self.finished(api::response_event::stream_finished::Reason::Done(
                api::response_event::stream_finished::Done {},
            ))],
            BridgeEvent::RunSettled { .. } => vec![self.finished(api::response_event::stream_finished::Reason::Done(
                api::response_event::stream_finished::Done {},
            ))],
            BridgeEvent::RunCancelled { .. } => Vec::new(),
            BridgeEvent::RunFailed { code, message, .. } => vec![self.finished(failure_reason(code, message))],
            BridgeEvent::ProtocolError { code, message } => vec![self.finished(failure_reason(code, message))],
            BridgeEvent::Diagnostic { .. } => Vec::new(),
        }
    }

    fn write_text_delta(&mut self, message_id: &str, delta: &str) -> Vec<api::ResponseEvent> {
        if delta.is_empty() {
            return Vec::new();
        }
        if self.created_messages.contains_key(message_id) {
            self.streamed_messages.insert(message_id.to_string(), ());
            let message = api::Message {
                id: message_id.to_string(),
                task_id: self.task_id.clone(),
                message: Some(api::message::Message::AgentOutput(api::message::AgentOutput {
                    text: delta.to_string(),
                })),
                ..Default::default()
            };
            return vec![client_actions(vec![
                api::client_action::Action::AppendToMessageContent(api::client_action::AppendToMessageContent {
                    task_id: self.task_id.clone(),
                    message: Some(message),
                    mask: Some(prost_types::FieldMask { paths: vec!["agent_output.text".to_string()] }),
                }),
            ])];
        }
        self.created_messages.insert(message_id.to_string(), true);
        self.streamed_messages.insert(message_id.to_string(), ());
        let message = api::Message {
            id: message_id.to_string(),
            task_id: self.task_id.clone(),
            request_id: self.request_id.clone(),
            timestamp: Some(now()),
            message: Some(api::message::Message::AgentOutput(api::message::AgentOutput {
                text: delta.to_string(),
            })),
            ..Default::default()
        };
        vec![client_actions(vec![api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask { task_id: self.task_id.clone(), messages: vec![message] },
        )])]
    }

    /// Synthesize a final text message for a message that never streamed
    /// (for example a non-streaming provider response).
    pub fn finish_with_text(&mut self, text: &str) -> Vec<api::ResponseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.fallback_message_counter += 1;
        let message_id = format!("{}:final{}", self.request_id, self.fallback_message_counter);
        self.write_text_delta(&message_id, text)
    }

    fn finished(&self, reason: api::response_event::stream_finished::Reason) -> api::ResponseEvent {
        api::ResponseEvent {
            r#type: Some(api::response_event::Type::Finished(api::response_event::StreamFinished {
                reason: Some(reason),
                token_usage: Vec::new(),
                should_refresh_model_config: false,
                #[allow(deprecated)]
                request_cost: None,
                conversation_usage_metadata: None,
                request_charges: None,
            })),
        }
    }
}

/// Map a bridge failure onto a Warp finish reason so the UI shows the right
/// error class instead of a generic failure.
pub fn failure_reason(
    code: &str,
    message: &str,
) -> api::response_event::stream_finished::Reason {
    use api::response_event::stream_finished as finished;
    match code {
        "invalid_api_key" | "auth" | "authentication_error" => {
            finished::Reason::InvalidApiKey(finished::InvalidApiKey {
                provider: 0,
                model_name: String::new(),
            })
        }
        "max_output_tokens" | "context_window_exceeded" => {
            finished::Reason::MaxTokenLimit(finished::ReachedMaxTokenLimit {})
        }
        "provider_error" | "llm_unavailable" | "timeout" => {
            finished::Reason::LlmUnavailable(finished::LlmUnavailable {})
        }
        _ => finished::Reason::InternalError(finished::InternalError { message: message.to_string() }),
    }
}

fn client_actions(actions: Vec<api::client_action::Action>) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::ClientActions(api::response_event::ClientActions {
            actions: actions
                .into_iter()
                .map(|action| api::ClientAction { action: Some(action) })
                .collect(),
        })),
    }
}

fn message_with_tool_call(task_id: &str, message_id: String, tool_call: api::message::ToolCall) -> api::Message {
    api::Message {
        id: message_id,
        task_id: task_id.to_string(),
        timestamp: Some(now()),
        message: Some(api::message::Message::ToolCall(tool_call)),
        ..Default::default()
    }
}

fn now() -> Timestamp {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp { seconds: since_epoch.as_secs() as i64, nanos: since_epoch.subsec_nanos() as i32 }
}

/// Convenience: map a stream of bridge events into a vector of Warp events.
/// Used by tests and by the app-layer stream adapter.
pub fn drain_to_warp_events(writer: &mut ExchangeWriter, events: Vec<BridgeEvent>) -> Vec<api::ResponseEvent> {
    let mut out = Vec::new();
    for event in events {
        out.extend(writer.write(&event));
    }
    out
}

/// Summarize an event stream in order (for assertions and diagnostics).
pub fn summarize_events(events: &[api::ResponseEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event.r#type.as_ref() {
            Some(api::response_event::Type::Init(_)) => "init".to_string(),
            Some(api::response_event::Type::ClientActions(actions)) => actions
                .actions
                .iter()
                .map(|action| {
                    match action.action.as_ref() {
                        Some(api::client_action::Action::CreateTask(_)) => "create_task",
                        Some(api::client_action::Action::AddMessagesToTask(_)) => "add_messages",
                        Some(api::client_action::Action::AppendToMessageContent(_)) => "append_text",
                        Some(api::client_action::Action::UpdateTaskMessage(_)) => "update_message",
                        Some(_) => "other_action",
                        None => "empty_action",
                    }
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("+"),
            Some(api::response_event::Type::Finished(_)) => "finished".to_string(),
            None => "empty".to_string(),
        })
        .collect()
}
