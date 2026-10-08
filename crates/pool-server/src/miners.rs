//! Miner connections: authenticated WebSocket, `Hello` handshake, then a
//! reader that forwards messages to the scheduler and a writer that drains the
//! scheduler's outbound queue and keeps the connection alive with pings.

use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use pool_protocol::messages::{MAX_MESSAGE_BYTES, MinerMessage, PROTOCOL_VERSION, PoolMessage, SUBPROTOCOL};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{AppState, error::ApiError};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_QUEUE: usize = 1024;

pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let label = state.auth.miner(&headers)?;
    Ok(upgrade
        .protocols([SUBPROTOCOL])
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(state, label, socket)))
}

async fn serve(state: AppState, label: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    let hello = tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await;
    let (miner_id, catalog_revision, hardware) = match hello {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<MinerMessage>(&text) {
            Ok(MinerMessage::Hello { protocol_version, miner_id, catalog_revision, hardware, miner_version }) => {
                if protocol_version != PROTOCOL_VERSION {
                    let error = PoolMessage::Error {
                        code: "protocol_mismatch".into(),
                        message: format!("pool speaks protocol {PROTOCOL_VERSION}, miner {protocol_version}"),
                    };
                    let _ = sink.send(text_message(&error)).await;
                    return;
                }
                debug!(%label, %miner_id, %miner_version, "miner hello");
                (miner_id, catalog_revision, hardware)
            }
            _ => {
                warn!(%label, "first miner message was not a valid hello");
                return;
            }
        },
        _ => {
            warn!(%label, "miner did not say hello");
            return;
        }
    };
    if *catalog_revision != *state.catalog_revision {
        // Models are still checked one by one against the catalog in Capacity.
        warn!(%label, %miner_id, "miner started with a different catalog revision; restart it to pick up changes");
    }

    let session = format!("ses_{}", uuid::Uuid::new_v4().simple());
    let welcome =
        PoolMessage::Welcome { session_id: session.clone(), heartbeat_seconds: state.config.heartbeat.as_secs() };
    if sink.send(text_message(&welcome)).await.is_err() {
        return;
    }

    let (outbound, mut outbound_rx) = mpsc::channel::<PoolMessage>(OUTBOUND_QUEUE);
    state.scheduler.miner_connected(&session, &label, &miner_id, hardware, outbound);

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.tick().await;
        loop {
            tokio::select! {
                message = outbound_rx.recv() => match message {
                    Some(message) => {
                        if sink.send(text_message(&message)).await.is_err() {
                            break;
                        }
                    }
                    // the scheduler dropped this miner
                    None => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => match serde_json::from_str::<MinerMessage>(&text) {
                Ok(message) => state.scheduler.miner_message(&session, message),
                Err(error) => {
                    warn!(%session, %error, "malformed miner message; closing");
                    break;
                }
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    info!(%session, %label, "miner connection closed");
    state.scheduler.miner_gone(&session);
    writer.abort();
}

fn text_message(message: &PoolMessage) -> Message {
    Message::Text(serde_json::to_string(message).expect("pool messages serialize").into())
}
