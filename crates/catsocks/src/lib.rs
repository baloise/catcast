//! CatCast WebSocket relay — a Cloudflare Worker.
//!
//! Topology: every connection is upgraded to a Durable Object named after
//! the room (the path segment after `/r/`). Inside the DO, every message
//! received from one socket is broadcast verbatim to every *other* socket
//! in the same room. The relay never sees plaintext — payloads are
//! AEAD-encrypted end-to-end between catstage and catc.
//!
//! Hibernatable WebSockets are used so the DO is billed only when there's
//! actual traffic. Idle rooms cost nothing.
//!
//! Native (non-wasm32) builds compile this crate as an empty placeholder so
//! that `cargo build --workspace` and the CI matrix on Linux/Windows keep
//! working. The Worker code below is only included when targeting
//! `wasm32-unknown-unknown`.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

#[cfg(target_arch = "wasm32")]
mod worker_impl {
    use uuid::Uuid;
    use worker::*;

    /// Entry point. Routes `/r/<room>` to a Durable Object named `<room>`,
    /// everything else gets a 404.
    #[event(fetch)]
    pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
        console_error_panic_hook::set_once();
        let url = req.url()?;
        let path = url.path().trim_matches('/').to_string();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() != 2 || parts[0] != "r" || parts[1].is_empty() {
            return Response::error("expected /r/<room>", 404);
        }
        let room = parts[1].to_string();
        let ns = env.durable_object("ROOM")?;
        let id = ns.id_from_name(&room)?;
        id.get_stub()?.fetch_with_request(req).await
    }

    /// One Durable Object per room. Holds N WebSockets, broadcasts every
    /// received text frame to all *other* sockets.
    #[durable_object]
    pub struct Room {
        state: State,
    }

    #[durable_object]
    impl DurableObject for Room {
        fn new(state: State, _env: Env) -> Self {
            console_error_panic_hook::set_once();
            Self { state }
        }

        async fn fetch(&mut self, _req: Request) -> Result<Response> {
            let pair = WebSocketPair::new()?;
            // Tag each socket with a unique id so we can later identify the
            // sender and skip it during broadcast.
            let tag = Uuid::new_v4().to_string();
            self.state
                .accept_web_socket_with_tags(&pair.server, vec![tag]);
            Response::from_websocket(pair.client)
        }

        async fn websocket_message(
            &mut self,
            ws: WebSocket,
            msg: WebSocketIncomingMessage,
        ) -> Result<()> {
            let text = match msg {
                WebSocketIncomingMessage::String(s) => s,
                // The protocol is text-only; binary frames are ignored.
                WebSocketIncomingMessage::Binary(_) => return Ok(()),
            };
            let sender_tags = self.state.get_tags(&ws);
            let sender_id = sender_tags.first();
            for other in self.state.get_websockets() {
                let other_tags = self.state.get_tags(&other);
                if other_tags.first() == sender_id {
                    continue;
                }
                // Best effort — a dead socket here shouldn't tank the relay.
                let _ = other.send_with_str(&text);
            }
            Ok(())
        }

        async fn websocket_close(
            &mut self,
            _ws: WebSocket,
            _code: usize,
            _reason: String,
            _was_clean: bool,
        ) -> Result<()> {
            Ok(())
        }

        async fn websocket_error(&mut self, _ws: WebSocket, _err: Error) -> Result<()> {
            Ok(())
        }
    }
}
