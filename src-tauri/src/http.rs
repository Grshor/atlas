//! HTTP API — endpoint-for-endpoint port of atlas/server.py (frozen contract).
//!
//! The router and every domain handler live in the [`web`] module tree
//! (`web/mod.rs` assembles the router; `web/<domain>.rs` holds one domain's
//! handlers and its tests). This file keeps the module declaration plus
//! re-exports so external call sites (`main.rs`, tests) keep using
//! `crate::http::*` unchanged.

#[path = "web/mod.rs"]
pub mod web;

pub use web::{bind, serve, spawn_assets_watcher, spawn_theme_watcher, AppState};
