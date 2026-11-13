//! User-interface adapters. All of them implement [`crate::types::UiGateway`]
//! so the agent loop stays independent of terminal, headless, and HTTP shells.

pub mod headless;
