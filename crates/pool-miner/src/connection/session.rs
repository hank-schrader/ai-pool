use super::*;

impl Miner {
    /// Connects, serves, and reconnects with backoff until shutdown.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let end = self.clone().session().await;
            *self.out.lock().unwrap() = None;
            self.abort_all().await;
            match end {
                Ok(End::Shutdown) => return,
                Ok(End::Lost(reason)) | Err(reason) => {
                    if self.shutdown.is_cancelled() {
                        return;
                    }
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    let jitter = Duration::from_millis((uuid::Uuid::new_v4().as_u128() % 1000) as u64);
                    tracing::warn!(
                        "pool connection: {reason}; reconnecting in {:.1}s",
                        (backoff + jitter).as_secs_f32()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff + jitter) => {}
                        _ = self.shutdown.cancelled() => return,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    pub(super) async fn session(self: Arc<Self>) -> Result<End, String> {
        let mut request = self.pool.connect_url().into_client_request().map_err(|error| error.to_string())?;
        request.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(SUBPROTOCOL));
        if let Some(token) = self.pool.token_value() {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| "invalid miner token")?;
            request.headers_mut().insert(header::AUTHORIZATION, value);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), true);
        let (socket, _) = tokio::time::timeout(Duration::from_secs(15), connect)
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|error| format!("cannot connect to {}: {error}", self.pool.connect_url()))?;
        let (mut sink, mut stream) = socket.split();

        let hello = MinerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            miner_id: self.miner_id.clone(),
            miner_version: env!("CARGO_PKG_VERSION").into(),
            catalog_revision: self.catalog_revision.clone(),
            hardware: self.hardware.clone(),
        };
        sink.send(text(&hello)).await.map_err(|error| error.to_string())?;
        let heartbeat_seconds = loop {
            let frame = tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .map_err(|_| "no welcome from the pool".to_string())?
                .ok_or("connection closed before welcome")?
                .map_err(|error| error.to_string())?;
            match frame {
                Message::Text(body) => match serde_json::from_str::<PoolMessage>(&body) {
                    Ok(PoolMessage::Welcome { session_id, heartbeat_seconds }) => {
                        tracing::info!("connected to {} (session {session_id})", self.pool.url());
                        break heartbeat_seconds.max(1);
                    }
                    Ok(PoolMessage::Error { code, message }) => {
                        return Err(format!("pool refused the miner: {code}: {message}"));
                    }
                    _ => return Err(format!("unexpected first message: {body}")),
                },
                Message::Close(frame) => return Err(format!("pool closed the connection: {frame:?}")),
                _ => {}
            }
        };

        let (out, mut outbox) = mpsc::channel::<MinerMessage>(256);
        *self.out.lock().unwrap() = Some(out.clone());
        let writer = tokio::spawn(async move {
            while let Some(message) = outbox.recv().await {
                if sink.send(text(&message)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        if self.draining.load(Ordering::SeqCst) {
            let _ = out.send(MinerMessage::Draining).await;
        }
        let _ = out.send(self.capacity()).await;
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_seconds));
        heartbeat.tick().await;

        let end = loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break Ok(End::Shutdown),
                _ = heartbeat.tick() => {
                    let active_attempts = self.active.lock().unwrap().keys().cloned().collect();
                    if out.send(MinerMessage::Heartbeat { active_attempts }).await.is_err() {
                        break Ok(End::Lost("writer stopped".into()));
                    }
                }
                changed = changes.changed() => {
                    if changed.is_ok() {
                        let _ = out.send(self.capacity()).await;
                    }
                }
                frame = stream.next() => match frame {
                    None => break Ok(End::Lost("connection closed".into())),
                    Some(Err(error)) => break Ok(End::Lost(error.to_string())),
                    Some(Ok(Message::Text(body))) => match serde_json::from_str::<PoolMessage>(&body) {
                        Ok(PoolMessage::Error { code, message }) => break Ok(End::Lost(format!("pool error {code}: {message}"))),
                        Ok(message) => self.handle(message, &out).await,
                        Err(error) => tracing::warn!("ignoring unknown pool message ({error}): {}", truncate(&body, 200)),
                    },
                    Some(Ok(Message::Close(frame))) => break Ok(End::Lost(format!("pool closed the connection: {frame:?}"))),
                    Some(Ok(_)) => {}
                },
            }
        };
        drop(out);
        writer.abort();
        end
    }
}
