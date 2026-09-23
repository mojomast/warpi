//! The M1 vertical slice, end to end, at the Warp event boundary:
//!
//! native request -> fixture HTTP provider -> Pi tool call -> brokered Warp
//! tool call -> Warp result -> SAME Pi turn resumes -> second provider request
//! -> final text -> Warp events.
//!
//! Everything except the Warp UI itself is real: the shipped helper binary
//! (Node + pinned Pi SDK), the stdio protocol, the Rust bridge, the Warp
//! protobuf event translation, and the deterministic provider HTTP fixture.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, StandaloneBridge};
use standalone_agent::protocol::HelperToolResultStatus;
use standalone_agent::warp_events::{
    ExchangeWriter, RequestInputs, extract_request_inputs, summarize_events,
};
use support::*;
use warp_multi_agent_api as api;

const TIMEOUT: Duration = Duration::from_secs(30);

fn writer_for(inputs: &RequestInputs, request_id: &str, run_id: &str) -> ExchangeWriter {
    ExchangeWriter::new(
        inputs.task_id.clone(),
        inputs,
        request_id.to_string(),
        run_id.to_string(),
    )
}

/// Assert the Warp event stream for the first (paused) exchange.
fn assert_paused_exchange(events: &[api::ResponseEvent], expected_command: &str) {
    let summary = summarize_events(events);
    assert_eq!(
        summary,
        vec!["init", "create_task", "add_messages", "finished"],
        "exchange event order"
    );

    let tool_message = events
        .iter()
        .find_map(|event| match event.r#type.as_ref() {
            Some(api::response_event::Type::ClientActions(actions)) => actions
                .actions
                .iter()
                .find_map(|action| match action.action.as_ref() {
                    Some(api::client_action::Action::AddMessagesToTask(add)) => {
                        add.messages.first().cloned()
                    }
                    _ => None,
                }),
            _ => None,
        })
        .expect("a tool-call message exists");
    let tool_call = tool_message.message.as_ref().expect("message payload");
    let api::message::Message::ToolCall(call) = tool_call else {
        panic!("expected a tool call, got {tool_call:?}");
    };
    assert_eq!(call.tool_call_id, "call_abc");
    let Some(api::message::tool_call::Tool::RunShellCommand(shell)) = call.tool.as_ref() else {
        panic!("expected a shell tool call");
    };
    assert_eq!(shell.command, expected_command);
    assert!(
        !shell.is_read_only,
        "read-only must not be assumed from the model"
    );
    assert_eq!(
        shell.risk_category,
        api::RiskCategory::NontrivialLocalChange as i32,
        "shell calls are classified for the approval path"
    );
}

// Uses the cross-process fixture provider, which is unreliable on Windows;
// `fixture_test!` ignores it there with the tracked reason (tests/support/mod.rs).
fixture_test! {
async fn native_prompt_tool_result_second_request_and_final_text() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_abc",
            "toolName": "bash",
            "argumentChunks": ["{\"comm", "and\":", "\"echo hel", "lo\"}"]
        },
        { "kind": "text", "chunks": ["all", " done"] }
    ]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge: StandaloneBridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;

    // Exchange 1: user query -> Pi pauses on our custom `bash` tool.
    let mut stream = bridge
        .start_turn("conv-1", "run echo hello".to_string())
        .await
        .expect("turn starts")
        .stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    let calls = tool_calls(&paused);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id, "call_abc");
    assert_eq!(
        shell_command(&calls[0]),
        Some("echo hello"),
        "the bridge translates the model call before forwarding it"
    );
    assert!(
        paused
            .iter()
            .any(|event| matches!(event, BridgeEvent::MessageUsage { .. })),
        "the bridge forwards assistant.usage for the tool-call message"
    );

    let inputs = RequestInputs {
        conversation_id: "conv-1".into(),
        task_id: "conv-1".into(),
        ..Default::default()
    };
    let mut writer = writer_for(&inputs, "req-1", "run-1");
    let warp_events: Vec<api::ResponseEvent> = paused
        .iter()
        .flat_map(|event| writer.write(event))
        .collect();
    assert_paused_exchange(&warp_events, "echo hello");

    // Exchange 2: the Warp tool result resumes the SAME Pi turn.
    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_abc",
                HelperToolResultStatus::Success,
                "hello\n",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(text_deltas(&settled), "all done");
    assert_eq!(final_text(&settled).as_deref(), Some("all done"));
    let settled_usage = usage(&settled).expect("usage recorded");
    assert!(settled_usage.input_tokens > 0);
    let message_usage = settled
        .iter()
        .find_map(|event| match event {
            BridgeEvent::MessageUsage { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("assistant.usage is forwarded");
    assert!(message_usage.input_tokens > 0);
    assert!(
        settled
            .iter()
            .any(|event| matches!(event, BridgeEvent::ContextUpdated { .. })),
        "context.updated is forwarded after the message"
    );

    let mut writer = writer_for(&inputs, "req-2", "run-1");
    let warp_events: Vec<api::ResponseEvent> = settled
        .iter()
        .flat_map(|event| writer.write(event))
        .collect();
    let summary = summarize_events(&warp_events);
    assert_eq!(summary.first().map(String::as_str), Some("init"));
    assert!(
        summary
            .iter()
            .filter(|entry| *entry == "add_messages")
            .count()
            == 1,
        "first delta creates exactly one message: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .filter(|entry| *entry == "append_text")
            .count()
            == 1,
        "second delta appends to it: {summary:?}"
    );
    assert_eq!(summary.last().map(String::as_str), Some("finished"));

    // The continuation must contain the tool result exactly once and no
    // duplicated user history.
    let captures = fixture.captures().await;
    let requests = captures.as_array().expect("captures array");
    assert_eq!(
        requests.len(),
        2,
        "exactly two provider requests: {captures}"
    );
    let second = &requests[1];
    let messages = second["body"]["messages"].as_array().expect("messages");
    let user_messages = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .count();
    assert_eq!(
        user_messages, 1,
        "user history is not duplicated: {messages:?}"
    );
    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(
        tool_messages.len(),
        1,
        "one tool result present: {messages:?}"
    );
    assert_eq!(tool_messages[0]["tool_call_id"], "call_abc");
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("hello"),
        "tool result content is delivered"
    );
    assert_eq!(second["headers"]["authorization"], "Bearer sk-fixture");

    bridge.shutdown().await;
}
}

fixture_test! {
async fn auth_none_never_sends_an_authorization_header() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([{ "kind": "text", "chunks": ["hello"] }]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(dir.path(), profile(&fixture.base_url, false), None).await;
    let mut stream = bridge
        .start_turn("conv-1", "hi".to_string())
        .await
        .expect("turn starts")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    let captures = fixture.captures().await;
    let headers = &captures[0]["headers"];
    for (name, value) in headers.as_object().expect("headers object") {
        let lower = name.to_ascii_lowercase();
        assert_ne!(
            lower, "authorization",
            "auth=none must not send Authorization ({value})"
        );
    }
    assert!(
        !captures.to_string().contains("warpi-no-auth"),
        "the internal placeholder must never reach the wire"
    );
    bridge.shutdown().await;
}
}

fixture_test! {
async fn foreign_and_duplicate_tool_results_are_rejected_without_corrupting_the_turn() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_abc",
            "toolName": "read",
            "argumentChunks": ["{\"path\":\"a.txt\"}"]
        },
        { "kind": "text", "chunks": ["done"] }
    ]);
    let fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;
    let mut stream = bridge
        .start_turn("conv-1", "read a.txt".to_string())
        .await
        .expect("turn starts")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    // A result for a tool call that belongs to no pending call must be
    // reported as a protocol error, and the real pending call must survive.
    let mut foreign = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "someone-else",
                HelperToolResultStatus::Success,
                "x",
            )],
        )
        .await
        .expect("resume call accepted")
        .stream;
    let events = collect_until(&mut foreign, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ProtocolError { .. })
    })
    .await;
    match events.last() {
        Some(BridgeEvent::ProtocolError { code, .. }) => assert_eq!(code, "unknown_tool_result"),
        other => panic!("expected protocol error, got {other:?}"),
    }

    // The genuine result still resumes the turn.
    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_abc",
                HelperToolResultStatus::Success,
                "file contents",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;

    // A duplicate delivery after the run settled must not launch anything.
    let duplicate = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_abc",
                HelperToolResultStatus::Success,
                "again",
            )],
        )
        .await;
    match duplicate {
        Ok(_) => panic!("a settled turn must not accept tool results"),
        Err(error) => assert!(
            error.to_string().contains("no active Pi turn"),
            "unexpected duplicate error: {error}"
        ),
    }
    bridge.shutdown().await;
}
}

fixture_test! {
async fn cancellation_settles_the_run_and_keeps_the_session_usable() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_hang",
            "toolName": "bash",
            "argumentChunks": ["{\"command\":\"sleep 30\"}"]
        },
        { "kind": "tool_call", "toolCallId": "call_two", "toolName": "glob", "argumentChunks": ["{\"pattern\":\"*.txt\"}"] },
        { "kind": "text", "chunks": ["second turn done"] }
    ]);
    let fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;
    let started = bridge
        .start_turn("conv-1", "start a long command".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    bridge
        .cancel_turn("conv-1", Some(&started.exchange_id))
        .await
        .expect("cancel accepted");
    let cancelled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunCancelled { .. })
    })
    .await;
    assert!(matches!(
        cancelled.last(),
        Some(BridgeEvent::RunCancelled { .. })
    ));

    // A fresh user message starts a new turn in the same session.
    let mut stream = bridge
        .start_turn("conv-1", "second turn".to_string())
        .await
        .expect("second turn starts")
        .stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    let calls = tool_calls(&paused);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id, "call_two");
    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_two",
                HelperToolResultStatus::Rejected,
                "no",
            )],
        )
        .await
        .expect("rejection accepted")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("second turn done"));
    bridge.shutdown().await;
}
}

fixture_test! {
async fn queued_prompts_are_accepted_and_start_in_fifo_order() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    // Request 1 pauses turn one on a tool call; the resume settles it. The two
    // prompts queued behind it then run as their own turns, in arrival order.
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_hold",
            "toolName": "bash",
            "argumentChunks": ["{\"command\":\"echo hold\"}"]
        },
        { "kind": "text", "chunks": ["first turn done"] },
        { "kind": "text", "chunks": ["second queued done"] },
        { "kind": "text", "chunks": ["third queued done"] }
    ]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;

    let started = bridge
        .start_turn("conv-1", "first query".to_string())
        .await
        .expect("first turn starts");
    let mut first_stream = started.stream;
    collect_until(&mut first_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    // Both prompts arrive while the first turn is still running: they are
    // accepted (no busy error) and neither starts yet.
    let second = bridge
        .start_turn("conv-1", "second query".to_string())
        .await
        .expect("a prompt submitted mid-turn must be accepted");
    let third = bridge
        .start_turn("conv-1", "third query".to_string())
        .await
        .expect("a second queued prompt must be accepted");
    let mut second_stream = second.stream;
    let mut third_stream = third.stream;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), second_stream.recv())
            .await
            .is_err(),
        "a queued prompt must not start while the running turn is unfinished"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), third_stream.recv())
            .await
            .is_err(),
        "a queued prompt must not start while the running turn is unfinished"
    );

    // Settling the first turn must start the queued prompts automatically.
    let mut resumed = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_hold",
                HelperToolResultStatus::Success,
                "held",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut resumed, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("first turn done"));

    let second_events = collect_until(&mut second_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert!(
        matches!(second_events.first(), Some(BridgeEvent::Init { .. })),
        "the queued exchange starts with its own init: {second_events:?}"
    );
    assert_eq!(
        final_text(&second_events).as_deref(),
        Some("second queued done")
    );

    let third_events = collect_until(&mut third_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(
        final_text(&third_events).as_deref(),
        Some("third queued done")
    );

    // One provider request per turn, and the queued prompts reached the model in
    // arrival order: the third prompt must not appear before the second.
    let captures = fixture.captures().await;
    let requests = captures.as_array().expect("captures array");
    assert_eq!(
        requests.len(),
        4,
        "one provider request per turn: {captures}"
    );
    let second_request = requests[2]["body"]["messages"].to_string();
    let third_request = requests[3]["body"]["messages"].to_string();
    assert!(
        second_request.contains("second query"),
        "FIFO order: {second_request}"
    );
    assert!(
        !second_request.contains("third query"),
        "the third prompt must not jump the queue: {second_request}"
    );
    assert!(
        third_request.contains("third query"),
        "FIFO order: {third_request}"
    );

    bridge.shutdown().await;
}
}

fixture_test! {
async fn cancelling_a_queued_prompt_settles_it_without_touching_the_running_turn() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_hold",
            "toolName": "bash",
            "argumentChunks": ["{\"command\":\"echo hold\"}"]
        },
        { "kind": "text", "chunks": ["first turn done"] }
    ]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;

    let started = bridge
        .start_turn("conv-1", "first query".to_string())
        .await
        .expect("first turn starts");
    let mut first_stream = started.stream;
    collect_until(&mut first_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    let queued = bridge
        .start_turn("conv-1", "queued query".to_string())
        .await
        .expect("queued prompt accepted");
    let mut queued_stream = queued.stream;

    // Cancelling the queued exchange settles it and leaves the running turn open
    // for its tool result.
    bridge
        .cancel_turn("conv-1", Some(&queued.exchange_id))
        .await
        .expect("cancel accepted");
    let queued_events = collect_until(&mut queued_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunCancelled { .. })
    })
    .await;
    assert!(matches!(
        queued_events.last(),
        Some(BridgeEvent::RunCancelled { .. })
    ));

    let mut resumed = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_hold",
                HelperToolResultStatus::Success,
                "held",
            )],
        )
        .await
        .expect("the running turn still accepts its tool result")
        .stream;
    let settled = collect_until(&mut resumed, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("first turn done"));

    // No queued prompt is left to run, so no further provider request happens.
    let captures = fixture.captures().await;
    assert_eq!(
        captures.as_array().map(Vec::len),
        Some(2),
        "a cancelled queued prompt must never reach the provider: {captures}"
    );
    bridge.shutdown().await;
}
}

fixture_test! {
async fn provider_failures_surface_as_a_single_terminal_failure() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([{ "kind": "error", "status": 401, "body": "{\"error\":{\"message\":\"bad key\"}}" }]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge =
        spawn_bridge(dir.path(), profile(&fixture.base_url, true), Some("sk-bad")).await;
    let mut stream = bridge
        .start_turn("conv-1", "hello".to_string())
        .await
        .expect("turn starts")
        .stream;
    let failed = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunFailed { .. })
    })
    .await;
    match failed.last() {
        Some(BridgeEvent::RunFailed {
            code, retryable, ..
        }) => {
            assert!(!retryable, "a 401 must not be retried");
            assert!(
                code == "provider_error" || code == "internal_error",
                "code: {code}"
            );
        }
        other => panic!("expected a terminal failure, got {other:?}"),
    }
    // No automatic retries: exactly one provider request.
    let captures = fixture.captures().await;
    assert_eq!(
        captures.as_array().map(Vec::len),
        Some(1),
        "captures: {captures}"
    );
    bridge.shutdown().await;
}
}

#[tokio::test]
async fn request_extraction_reads_the_native_request_shape() {
    let request = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: vec![api::Task {
                id: "root".into(),
                ..Default::default()
            }],
        }),
        input: Some(api::request::Input {
            context: Some(api::InputContext {
                directory: Some(api::input_context::Directory {
                    pwd: "/work".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            r#type: Some(api::request::input::Type::UserInputs(
                api::request::input::UserInputs {
                    inputs: vec![api::request::input::user_inputs::UserInput {
                        input: Some(
                            api::request::input::user_inputs::user_input::Input::UserQuery(
                                api::request::input::UserQuery {
                                    query: "fix the test".into(),
                                    ..Default::default()
                                },
                            ),
                        ),
                    }],
                },
            )),
        }),
        ..Default::default()
    };
    let inputs = extract_request_inputs(&request).expect("extracts");
    assert_eq!(inputs.conversation_id, "root");
    assert_eq!(inputs.user_query.as_deref(), Some("fix the test"));
    assert_eq!(inputs.working_dir.as_deref(), Some("/work"));
    assert!(inputs.tool_results.is_empty());
}

fixture_test! {
async fn a_server_backed_task_is_not_upgraded_again() {
    // Regression: re-sending CreateTask for a task the client already treats as
    // server-backed fails with `UnexpectedUpgrade`, so the bridge must only emit
    // it when the app layer says the conversation is new.
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_1",
            "toolName": "bash",
            "argumentChunks": ["{\"command\":\"echo hi\"}"]
        },
        { "kind": "text", "chunks": ["done"] }
    ]);
    let fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge_with_task(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
        "existing-task",
        false,
    )
    .await;
    let mut stream = bridge
        .start_turn("conv-1", "run echo hi".to_string())
        .await
        .expect("turn starts")
        .stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    assert!(
        !paused
            .iter()
            .any(|event| matches!(event, BridgeEvent::CreateTask { .. })),
        "a server-backed task must not be upgraded again"
    );
    let inputs = RequestInputs {
        conversation_id: "conv-1".into(),
        task_id: "existing-task".into(),
        ..Default::default()
    };
    let mut writer = writer_for(&inputs, "req-1", "run-1");
    let warp_events: Vec<api::ResponseEvent> = paused
        .iter()
        .flat_map(|event| writer.write(event))
        .collect();
    let summary = summarize_events(&warp_events);
    assert_eq!(
        summary,
        vec!["init", "add_messages", "finished"],
        "no create_task: {summary:?}"
    );
    bridge.shutdown().await;
}
}

#[tokio::test]
async fn new_conversations_without_a_task_context_are_accepted() {
    // Regression: the native client sends no task context for a brand-new
    // conversation (the real server generates the task id). The adapter must
    // accept that instead of failing the request.
    let request = api::Request {
        task_context: None,
        input: Some(api::request::Input {
            r#type: Some(api::request::input::Type::UserInputs(
                api::request::input::UserInputs {
                    inputs: vec![api::request::input::user_inputs::UserInput {
                        input: Some(
                            api::request::input::user_inputs::user_input::Input::UserQuery(
                                api::request::input::UserQuery {
                                    query: "hello".into(),
                                    ..Default::default()
                                },
                            ),
                        ),
                    }],
                },
            )),
            ..Default::default()
        }),
        ..Default::default()
    };
    let inputs = extract_request_inputs(&request).expect("extracts without a task context");
    assert!(
        inputs.task_id.is_empty(),
        "the caller generates the task id"
    );
    assert_eq!(inputs.user_query.as_deref(), Some("hello"));

    // A request with tasks still yields the root task id.
    let with_tasks = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: vec![api::Task {
                id: "root-task".into(),
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    let inputs = extract_request_inputs(&with_tasks).expect("extracts");
    assert_eq!(inputs.task_id, "root-task");
    assert_eq!(inputs.conversation_id, "root-task");
}

#[tokio::test]
async fn tool_result_rendering_maps_shell_results_for_the_model() {
    let result = api::message::ToolCallResult {
        tool_call_id: "call-1".into(),
        context: None,
        result: Some(api::message::tool_call_result::Result::RunShellCommand(
            api::RunShellCommandResult {
                command: "echo hi".into(),
                result: Some(api::run_shell_command_result::Result::CommandFinished(
                    api::ShellCommandFinished {
                        output: "hi\n".into(),
                        exit_code: 0,
                        command_id: String::new(),
                        start_ts: None,
                        finish_ts: None,
                    },
                )),
                ..Default::default()
            },
        )),
    };
    let (status, text) = standalone_agent::warp_events::render_tool_call_result(&result);
    assert_eq!(
        status,
        standalone_agent::warp_events::RenderedStatus::Success
    );
    assert!(text.contains("hi"));
    assert!(text.contains("exit code: 0"));

    let denied = api::message::ToolCallResult {
        tool_call_id: "call-2".into(),
        context: None,
        result: Some(api::message::tool_call_result::Result::RunShellCommand(
            api::RunShellCommandResult {
                command: "rm -rf /".into(),
                result: Some(api::run_shell_command_result::Result::PermissionDenied(
                    api::PermissionDenied { reason: None },
                )),
                ..Default::default()
            },
        )),
    };
    let (status, text) = standalone_agent::warp_events::render_tool_call_result(&denied);
    assert_eq!(
        status,
        standalone_agent::warp_events::RenderedStatus::Rejected
    );
    assert!(text.contains("did not permit"));
}
