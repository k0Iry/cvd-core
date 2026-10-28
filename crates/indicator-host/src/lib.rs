//! Host-side runner supervision for CVD indicator libraries.
//!
//! The host never loads indicator libraries itself; it talks to
//! `cvd-indicator-runner` child processes over the framed protocol.

pub mod client;

pub use client::{HostError, RunnerClient, RunnerConfig};
