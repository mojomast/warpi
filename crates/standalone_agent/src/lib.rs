//! Standalone agent backend for the Warp fork.
//!
//! This crate is the thin Rust layer between Warp's native agent UI and a
//! supervised private helper that embeds the Pi coding-agent SDK:
//!
//! - [`provider`] holds the local-only provider/profile registry. Endpoint
//!   metadata never contains secrets.
//! - [`secrets`] resolves credentials from the platform secret store at
//!   session-open time.
//! - [`helper`] supervises the stdio helper process (sanitized environment,
//!   bounded frames, independent stderr drain, deterministic teardown).
//! - [`bridge`] owns the session/turn/exchange state machine and correlates
//!   tool results to the exact suspended Pi tool call.
//! - [`warp_events`] translates bridge events into the Warp multi-agent
//!   protobuf the native controller already understands.
//! - [`journal`] is the durable per-conversation prompt journal and recovery
//!   planner.
//! - [`event_log`] is the durable per-conversation event log behind the bridge's
//!   delivery modes.
//!
//! Ownership split: Pi owns the model loop and the canonical transcript; Warp
//! owns approvals and workspace execution; this crate owns only transcription
//! between the two.

pub mod bridge;
pub mod event_log;
pub mod helper;
pub mod journal;
pub mod protocol;
pub mod provider;
pub mod secrets;
pub mod usage_ledger;
pub mod warp_events;

pub use bridge::{
    BridgeConfig, BridgeError, BridgeEvent, BridgeTimeouts, RetryOptions, SessionSpec,
    StandaloneBridge, TurnStream,
};
pub use provider::{ChatCompletionsCompat, CredentialRef, ProviderProfile, WireProtocol};
pub use secrets::{SecretStore, SecretString};
