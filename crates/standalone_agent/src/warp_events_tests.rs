//! Unit tests for the Warp event translation layer.
//!
//! The integration tests in `tests/vertical_slice.rs` cover the full helper
//! round trip; these tests pin the model-facing rendering and tool translation
//! rules without spawning a helper.

use warp_multi_agent_api as api;

use crate::bridge::BridgeEvent;
use crate::protocol::ToolCallSpec;
use crate::warp_events::{
    ExchangeWriter, MAX_SHELL_OUTPUT_WAIT_SECONDS, RenderedStatus, RequestInputs,
    ToolTranslationError, render_request_tool_call_result, render_tool_call_result,
    summarize_events, translate_tool_call,
};

fn snapshot(output: &str, alt_screen: bool) -> api::LongRunningShellCommandSnapshot {
    api::LongRunningShellCommandSnapshot {
        output: output.to_string(),
        cursor: String::new(),
        command_id: "block-42".to_string(),
        is_alt_screen_active: alt_screen,
        is_preempted: false,
        activity: None,
    }
}

#[allow(deprecated)]
fn shell_message_result(
    result: api::run_shell_command_result::Result,
) -> api::message::ToolCallResult {
    api::message::ToolCallResult {
        tool_call_id: "call-1".to_string(),
        context: None,
        result: Some(api::message::tool_call_result::Result::RunShellCommand(
            api::RunShellCommandResult {
                command: "sleep 999".to_string(),
                output: String::new(),
                exit_code: 0,
                result: Some(result),
            },
        )),
    }
}

#[allow(deprecated)]
fn request_shell_result(
    result: api::run_shell_command_result::Result,
) -> api::request::input::ToolCallResult {
    api::request::input::ToolCallResult {
        tool_call_id: "call-1".to_string(),
        result: Some(
            api::request::input::tool_call_result::Result::RunShellCommand(
                api::RunShellCommandResult {
                    command: "sleep 999".to_string(),
                    output: String::new(),
                    exit_code: 0,
                    result: Some(result),
                },
            ),
        ),
    }
}

fn request_bash_output_result(
    result: api::read_shell_command_output_result::Result,
) -> api::request::input::ToolCallResult {
    api::request::input::ToolCallResult {
        tool_call_id: "call-2".to_string(),
        result: Some(
            api::request::input::tool_call_result::Result::ReadShellCommandOutput(
                api::ReadShellCommandOutputResult {
                    command: "sleep 999".to_string(),
                    result: Some(result),
                },
            ),
        ),
    }
}

fn bash_output_spec(arguments: serde_json::Value) -> ToolCallSpec {
    ToolCallSpec {
        tool_call_id: "call-2".to_string(),
        name: "workspace.read_shell_command_output".to_string(),
        arguments,
    }
}

#[test]
fn long_running_snapshot_is_an_error_with_actionable_guidance() {
    let result = shell_message_result(
        api::run_shell_command_result::Result::LongRunningCommandSnapshot(snapshot(
            "partial line\n",
            false,
        )),
    );

    let (status, text) = render_tool_call_result(&result);

    assert_eq!(
        status,
        RenderedStatus::Error,
        "a running command is not a success"
    );
    for expected in [
        "still running",
        "not a success",
        "block-42",
        "bash_output",
        "BatchMode",
        "timeout",
        "partial line",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in: {text}");
    }
}

#[test]
fn running_snapshot_keeps_partial_output_and_reports_the_command_id() {
    let result = request_shell_result(
        api::run_shell_command_result::Result::LongRunningCommandSnapshot(snapshot(
            "line one\nline two",
            false,
        )),
    );

    let (status, text) = render_request_tool_call_result(&result);

    assert_eq!(status, RenderedStatus::Error);
    assert!(
        text.contains("line one\nline two"),
        "partial output kept: {text}"
    );
    assert!(
        text.contains("\"block-42\""),
        "command id is actionable: {text}"
    );
}

#[test]
fn alt_screen_snapshot_warns_that_the_program_will_not_return() {
    let result = shell_message_result(
        api::run_shell_command_result::Result::LongRunningCommandSnapshot(snapshot("", true)),
    );

    let (status, text) = render_tool_call_result(&result);

    assert_eq!(status, RenderedStatus::Error);
    assert!(
        text.contains("alternate screen"),
        "alt-screen warning present: {text}"
    );
}

#[test]
fn finished_shell_command_still_maps_exit_codes_to_status() {
    let ok = shell_message_result(api::run_shell_command_result::Result::CommandFinished(
        api::ShellCommandFinished {
            output: "hi\n".to_string(),
            exit_code: 0,
            command_id: "block-42".to_string(),
            start_ts: None,
            finish_ts: None,
        },
    ));
    let (status, text) = render_tool_call_result(&ok);
    assert_eq!(status, RenderedStatus::Success);
    assert!(text.contains("hi"));
    assert!(text.contains("exit code: 0"));

    let failed = shell_message_result(api::run_shell_command_result::Result::CommandFinished(
        api::ShellCommandFinished {
            output: "boom\n".to_string(),
            exit_code: 2,
            command_id: "block-42".to_string(),
            start_ts: None,
            finish_ts: None,
        },
    ));
    let (status, _) = render_tool_call_result(&failed);
    assert_eq!(status, RenderedStatus::Error);
}

#[test]
fn polled_snapshot_is_an_error_and_completion_maps_exit_codes() {
    let snapshot_result = request_bash_output_result(
        api::read_shell_command_output_result::Result::LongRunningCommandSnapshot(snapshot(
            "tick\n", false,
        )),
    );
    let (status, text) = render_request_tool_call_result(&snapshot_result);
    assert_eq!(status, RenderedStatus::Error);
    assert!(text.contains("requested wait"), "{text}");

    let finished = request_bash_output_result(
        api::read_shell_command_output_result::Result::CommandFinished(api::ShellCommandFinished {
            output: "done\n".to_string(),
            exit_code: 0,
            command_id: "block-42".to_string(),
            start_ts: None,
            finish_ts: None,
        }),
    );
    let (status, text) = render_request_tool_call_result(&finished);
    assert_eq!(status, RenderedStatus::Success);
    assert!(text.contains("done"));
    assert!(text.contains("exit code: 0"));

    let missing = request_bash_output_result(api::read_shell_command_output_result::Result::Error(
        api::ShellCommandError {
            r#type: Some(api::shell_command_error::Type::CommandNotFound(())),
        },
    ));
    let (status, text) = render_request_tool_call_result(&missing);
    assert_eq!(status, RenderedStatus::Error);
    assert!(text.contains("No running command matches"), "{text}");
}

fn writer() -> ExchangeWriter {
    let inputs = RequestInputs {
        conversation_id: "conv-1".into(),
        task_id: "task-1".into(),
        ..Default::default()
    };
    ExchangeWriter::new("task-1".into(), &inputs, "request-1".into(), "run-1".into())
}

#[test]
fn a_helper_cancel_finishes_the_exchange_instead_of_ending_it_without_a_reason() {
    let mut writer = writer();

    let events = writer.write(&BridgeEvent::RunCancelled {
        reason: "cancelled by client".to_string(),
    });

    assert_eq!(summarize_events(&events), vec!["finished"]);
    let finished = events
        .iter()
        .find_map(|event| match event.r#type.as_ref() {
            Some(api::response_event::Type::Finished(finished)) => Some(finished),
            _ => None,
        })
        .expect("a terminal event");
    assert!(
        matches!(
            finished.reason,
            Some(api::response_event::stream_finished::Reason::Done(_))
        ),
        "the proto has no Cancelled reason; Done is the deterministic finish"
    );
}

#[test]
fn pi_shell_calls_are_marked_risky_so_the_redirection_gate_applies() {
    let spec = ToolCallSpec {
        tool_call_id: "call-pi".to_string(),
        name: "workspace.shell".to_string(),
        arguments: serde_json::json!({ "command": "echo x > ~/.bashrc" }),
    };

    let translated = translate_tool_call(&spec).expect("translates");

    let Some(api::message::tool_call::Tool::RunShellCommand(shell)) = translated.tool else {
        panic!("expected a RunShellCommand call");
    };
    assert!(
        shell.is_risky,
        "Pi cannot classify risk; `is_risky: false` would skip the redirection gate"
    );
    assert!(!shell.is_read_only);
}

#[test]
fn tool_calls_render_the_translated_calls_the_bridge_validated() {
    let spec = ToolCallSpec {
        tool_call_id: "call-1".to_string(),
        name: "workspace.shell".to_string(),
        arguments: serde_json::json!({ "command": "echo hi" }),
    };
    let translated = translate_tool_call(&spec).expect("translates");
    let mut writer = writer();

    let events = writer.write(&BridgeEvent::ToolCalls {
        calls: vec![translated],
    });

    assert_eq!(summarize_events(&events), vec!["add_messages"]);
    let message = events
        .iter()
        .find_map(|event| match event.r#type.as_ref() {
            Some(api::response_event::Type::ClientActions(actions)) => actions
                .actions
                .iter()
                .find_map(|action| match action.action.as_ref() {
                    Some(api::client_action::Action::AddMessagesToTask(add)) => {
                        add.messages.first()
                    }
                    _ => None,
                }),
            _ => None,
        })
        .expect("a tool-call message");
    let api::message::Message::ToolCall(call) = message.message.as_ref().expect("message payload")
    else {
        panic!("expected a tool call");
    };
    assert_eq!(call.tool_call_id, "call-1");
    assert!(
        matches!(
            call.tool,
            Some(api::message::tool_call::Tool::RunShellCommand(_))
        ),
        "the executor receives a representable call, never Tool::Server"
    );
}

#[test]
fn bash_output_maps_to_a_bounded_read_shell_command_output_call() {
    let spec =
        bash_output_spec(serde_json::json!({ "command_id": "block-42", "wait_seconds": 999 }));

    let tool_call = translate_tool_call(&spec).expect("translates");

    let Some(api::message::tool_call::Tool::ReadShellCommandOutput(read)) = tool_call.tool else {
        panic!("expected a ReadShellCommandOutput tool call");
    };
    assert_eq!(read.command_id, "block-42");
    match read.delay {
        Some(api::message::tool_call::read_shell_command_output::Delay::Duration(duration)) => {
            assert_eq!(
                duration.seconds, MAX_SHELL_OUTPUT_WAIT_SECONDS,
                "waits are capped"
            );
            assert_eq!(duration.nanos, 0);
        }
        other => panic!("expected a bounded duration, got {other:?}"),
    }
}

#[test]
fn bash_output_defaults_to_a_bounded_wait_and_requires_a_command_id() {
    let spec = bash_output_spec(serde_json::json!({ "command_id": "block-42" }));
    let tool_call = translate_tool_call(&spec).expect("translates");
    let Some(api::message::tool_call::Tool::ReadShellCommandOutput(read)) = tool_call.tool else {
        panic!("expected a ReadShellCommandOutput tool call");
    };
    match read.delay {
        Some(api::message::tool_call::read_shell_command_output::Delay::Duration(duration)) => {
            assert_eq!(duration.seconds, 30, "defaults to a short bounded wait");
        }
        other => panic!("expected a bounded duration, got {other:?}"),
    }

    let missing = bash_output_spec(serde_json::json!({ "wait_seconds": 10 }));
    assert!(matches!(
        translate_tool_call(&missing),
        Err(ToolTranslationError::InvalidArgument(name)) if name == "command_id"
    ));
}

#[test]
fn usage_and_context_bridge_events_do_not_emit_native_events_yet() {
    let inputs = RequestInputs {
        conversation_id: "conv-1".to_string(),
        task_id: "conv-1".to_string(),
        ..Default::default()
    };
    let mut writer = ExchangeWriter::new("conv-1".into(), &inputs, "req-1".into(), "run-1".into());
    let events = [
        BridgeEvent::MessageUsage {
            message_id: "m1".to_string(),
            model_id: "model".to_string(),
            usage: crate::protocol::AgentUsage::default(),
            duration_ms: 10,
            first_token_ms: Some(1),
            output_tokens_per_second: Some(1.0),
            stop_reason: "stop".to_string(),
        },
        BridgeEvent::ContextUpdated {
            tokens: Some(100),
            context_window: Some(1_000),
            percent: Some(10.0),
            source: "usage".to_string(),
        },
        BridgeEvent::CompactionFinished {
            reason: "threshold".to_string(),
            summarized: true,
            tokens_before: Some(900),
            tokens_after: Some(100),
            summary_usage: None,
            duration_ms: Some(5),
        },
    ];
    for event in events {
        assert!(
            writer.write(&event).is_empty(),
            "usage facts feed the app-side ledger; the native mapping is separate"
        );
    }
}

#[test]
fn subagent_task_events_do_not_emit_native_events_yet() {
    let mut writer = writer();
    let events = [
        BridgeEvent::TaskStarted {
            task_id: "task-1".to_string(),
            child_session_id: "conv-1:task-1".to_string(),
            description: "explore auth".to_string(),
            subagent_type: "explore".to_string(),
            prompt_bytes: 100,
            max_turns: 5,
            deadline_ms: 60_000,
            token_cap: 50_000,
        },
        BridgeEvent::TaskProgress {
            task_id: "task-1".to_string(),
            child_session_id: "conv-1:task-1".to_string(),
            elapsed_ms: 1_000,
            turns: 1,
            tool_calls: 0,
            tokens: crate::protocol::TaskProgressTokens {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
            },
            pending_tools: 0,
        },
        BridgeEvent::TaskCompleted {
            task_id: "task-1".to_string(),
            child_session_id: "conv-1:task-1".to_string(),
            status: "ok".to_string(),
            reason: None,
            subagent_type: "explore".to_string(),
            turns: 2,
            tool_calls: 3,
            usage: crate::protocol::AgentUsage {
                input_tokens: 1_000,
                output_tokens: 200,
                total_tokens: Some(1_200),
                ..Default::default()
            },
            wall_ms: 2_000,
            summary_bytes: 512,
        },
    ];
    for event in events {
        assert!(
            writer.write(&event).is_empty(),
            "task facts are diagnostic; the native mapping is separate"
        );
    }
}
