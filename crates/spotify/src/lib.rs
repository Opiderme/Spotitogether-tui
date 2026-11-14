//! Minimal Spotify Web API client for Spotitogether.
//!
//! Only the endpoints the app needs are implemented, against the
//! Development Mode restrictions in force since February 2026
//! (search limited to 10 results, no batch endpoints, no `product` on `/me`).

pub mod auth;
mod client;
mod error;
pub mod models;

pub use auth::Token;
pub use client::{Method, SpotifyClient, TokenSink};
pub use error::{Error, Result};
