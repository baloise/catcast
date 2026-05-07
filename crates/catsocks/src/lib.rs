//! CatCast WebSocket relay — Cloudflare Worker placeholder.
//!
//! The real implementation uses the `worker` crate's Durable Object support
//! with hibernatable WebSockets to broadcast every received message to every
//! other socket in the same room. Until that lands, this crate exists only
//! to keep the workspace shape stable.
