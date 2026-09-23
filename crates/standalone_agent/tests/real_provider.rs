//! Real-provider validation against a live OpenAI-compatible endpoint.
//!
//! This test is skipped unless credentials are provided through the
//! environment, so it never runs (or leaks anything) in normal CI:
//!
//! ```bash
//! WARPI_REAL_PROVIDER_KEY="$(cat /path/to/key)" \
//! WARPI_REAL_PROVIDER_BASE_URL="https://api.deepseek.com/v1" \
//! WARPI_REAL_PROVIDER_MODEL="deepseek-chat" \
//! cargo test -p standalone_agent --test real_provider -- --nocapture
//! ```
//!
//! What it proves: the shipped helper talks to a real provider, the model's
//! tool call is brokered back as a canonical workspace call, a tool result
//! resumes the same prompt, and the provider's second response contains the
//! result. The tool *execution* is performed by this test (the equivalent of
//! Warp running the approved command); the model loop and transcript are Pi's.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, StandaloneBridge};
use standalone_agent::protocol::HelperToolResultStatus;
use support::*;

const TIMEOUT: Duration = Duration::from_secs(180);
/// Marker the model is asked to reproduce from the tool output. It only exists
/// in this test file, so a match means the loop really closed.
const MARKER: &str = "warpi-real-provider-marker";

fn real_provider_config() -> Option<(String, String, String)> {
    let key = std::env::var("WARPI_REAL_PROVIDER_KEY").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    let base_url = std::env::var("WARPI_REAL_PROVIDER_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());
    let model =
        std::env::var("WARPI_REAL_PROVIDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());
    Some((key, base_url, model))
}

#[tokio::test]
async fn real_provider_tool_round_trip() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let Some((key, base_url, model)) = real_provider_config() else {
        eprintln!("NOT RUN: set WARPI_REAL_PROVIDER_KEY to run the real-provider test");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let mut profile = profile(&base_url, true);
    profile.id = "real-provider".into();
    profile.display_name = "Real provider".into();
    profile.model_id = model.clone();
    // DeepSeek's OpenAI-compatible endpoint accepts system-role messages and
    // reports usage in the final streamed chunk.
    profile.compat.supports_developer_role = false;
    profile.compat.supports_usage_in_streaming = true;
    profile.context_limit = 65536;
    profile.output_limit = 4096;

    let mut bridge: StandaloneBridge =
        spawn_bridge_with_task(dir.path(), profile, Some(key.as_str()), "real-task", true).await;

    let prompt = format!(
        "Use the bash tool to run exactly this command: echo {MARKER}\n\
         Then reply with a single line: RESULT=<the exact stdout of that command>"
    );
    let mut stream = bridge
        .start_turn("conv-1", prompt)
        .await
        .expect("turn starts")
        .stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(
            event,
            BridgeEvent::ExchangePaused { .. } | BridgeEvent::RunFailed { .. }
        )
    })
    .await;
    if let Some(BridgeEvent::RunFailed { code, message, .. }) = paused.last() {
        panic!("provider failed before the tool call: {code}: {message}");
    }
    let calls = tool_calls(&paused);
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one brokered tool call: {paused:?}"
    );
    let command = shell_command(&calls[0])
        .expect("the model call arrived as a translated shell command")
        .to_string();
    assert!(
        command.contains(MARKER),
        "the model should echo the requested marker: {command}"
    );

    // Execute the approved command exactly as Warp would, then resume.
    let output = run_marker_command(&command);
    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                &calls[0].tool_call_id,
                HelperToolResultStatus::Success,
                &output,
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(
            event,
            BridgeEvent::RunSettled { .. } | BridgeEvent::RunFailed { .. }
        )
    })
    .await;
    match settled.last() {
        Some(BridgeEvent::RunSettled { usage, .. }) => {
            assert!(
                usage.as_ref().is_some_and(|usage| usage.input_tokens > 0),
                "usage: {usage:?}"
            );
        }
        other => panic!("provider round trip did not settle: {other:?}"),
    }
    let final_text = final_text(&settled).unwrap_or_default();
    assert!(
        final_text.contains(MARKER),
        "the second provider response should carry the tool output back: {final_text:?}"
    );
    eprintln!(
        "real provider ({model}) final answer: {}",
        final_text.lines().next().unwrap_or_default()
    );
    bridge.shutdown().await;
}

/// Run the model-requested command locally, mirroring what Warp's approved
/// shell executor does, and return its combined output.
#[allow(clippy::disallowed_types)] // test-only helper: no console window exists under cargo test
fn run_marker_command(command: &str) -> String {
    // Only the marker command is expected here; refuse anything else so this
    // test can never be turned into an arbitrary command executor.
    assert!(
        command.contains(MARKER) && (command.starts_with("echo ") || command.ends_with(MARKER)),
        "refusing to execute an unexpected command in the test: {command}"
    );
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .expect("run command");
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        text.push_str(&format!(
            "\nexit code: {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    text
}
