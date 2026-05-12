//! `catsocks` — native build of the CatCast WebSocket broker.
//!
//! Same wire shape as the production CF Worker (`/r/<room>` WebSocket
//! upgrade, text frames broadcast to every other socket in the room),
//! implemented natively with axum/tokio so the inner dev loop has no Node
//! or wrangler dependency. In-memory rooms, single process, plain TCP —
//! intended for local dev, not for fronting real traffic.
//!
//! ```text
//! cargo run -p catsocks
//! # listens on ws://127.0.0.1:8787/r/<room>
//! ```

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::Response,
    routing::get,
    Router,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "catsocks", version, about = "Native CatCast broker (axum)")]
struct Args {
    /// Address to listen on. Defaults to 127.0.0.1:8787 to match `wrangler dev`.
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
}

type Tx = mpsc::Sender<String>;
type Room = Vec<(Uuid, Tx)>;
type RoomMap = HashMap<String, Room>;

/// In-memory room registry. Each socket gets a unique id so we can broadcast
/// to "everyone in this room except me" without echoing back to the sender.
#[derive(Clone, Default)]
struct Rooms {
    inner: Arc<Mutex<RoomMap>>,
}

impl Rooms {
    async fn join(&self, room: &str, tx: Tx) -> Uuid {
        let id = Uuid::new_v4();
        let mut g = self.inner.lock().await;
        g.entry(room.to_string()).or_default().push((id, tx));
        id
    }

    async fn leave(&self, room: &str, id: Uuid) {
        let mut g = self.inner.lock().await;
        if let Some(v) = g.get_mut(room) {
            v.retain(|(i, _)| *i != id);
            if v.is_empty() {
                g.remove(room);
            }
        }
    }

    /// Send `msg` to every socket in `room` except the one with id `sender`.
    async fn broadcast(&self, room: &str, sender: Uuid, msg: &str) {
        let recipients: Vec<Tx> = {
            let g = self.inner.lock().await;
            g.get(room)
                .map(|v| {
                    v.iter()
                        .filter(|(id, _)| *id != sender)
                        .map(|(_, tx)| tx.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        for tx in recipients {
            // Best effort — dropped if the recipient's queue is full or gone.
            let _ = tx.try_send(msg.to_string());
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let rooms = Rooms::default();

    let app = Router::new()
        .route("/r/{room}", get(ws_upgrade))
        .with_state(rooms);

    tracing::info!("catsocks listening on ws://{}/r/<room>", args.bind);
    let listener = match tokio::net::TcpListener::bind(args.bind).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            tracing::error!(
                "bind failed: {} is already in use; stop the existing process or run with --bind <host:port>",
                args.bind
            );
            std::process::exit(1);
        }
        Err(err) => {
            tracing::error!("bind failed on {}: {}", args.bind, err);
            std::process::exit(1);
        }
    };

    if let Err(err) = axum::serve(listener, app).await {
        tracing::error!("server error: {}", err);
        std::process::exit(1);
    }
}

async fn ws_upgrade(
    Path(room): Path<String>,
    State(rooms): State<Rooms>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, room, rooms))
}

async fn handle_socket(socket: WebSocket, room: String, rooms: Rooms) {
    let (mut sink, mut stream) = socket.split();

    // Per-socket outbound mailbox — sized small because messages are tiny
    // and we'd rather drop than block the broadcast fan-out.
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let my_id = rooms.join(&room, tx).await;
    tracing::debug!(%room, ?my_id, "socket joined");

    // Pump outbound mailbox -> websocket.
    let outbound = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read inbound -> broadcast.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => rooms.broadcast(&room, my_id, t.as_str()).await,
            Message::Close(_) => break,
            // Binary, ping, pong: not part of the protocol; ignore.
            _ => continue,
        }
    }

    rooms.leave(&room, my_id).await;
    outbound.abort();
    tracing::debug!(%room, ?my_id, "socket left");
}
