//! Aaykra Code — native Rust coding agent.
//!
//! This crate is intentionally split into a library and a thin binary so the
//! agent runtime can be embedded in Aaykra editor integrations, tests, and
//! headless automation.

pub mod agent;
pub mod agents;
pub mod auth;
pub mod bootstrap;
pub mod checkpoint;
pub mod cli;
pub mod commands;
pub mod config;
pub mod diff;
pub mod frontmatter;
pub mod hooks;
pub mod images;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod paths;
pub mod permissions;
pub mod providers;
pub mod repo_map;
pub mod runtime;
pub mod server;
pub mod session;
pub mod skills;
pub mod tools;
pub mod tui;
pub mod types;
pub mod ui;
