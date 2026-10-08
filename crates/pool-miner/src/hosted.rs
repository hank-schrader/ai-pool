//! One served model: its llama-server child kept alive by a supervisor, and,
//! for Clef, the token-count helper.

use std::{
    fs::File,
    sync::Arc,
    time::{Duration, Instant},
};

use pool_protocol::{Model, Profile, messages::LoadedModel};
use pool_runtime::{ClefCounter, LaunchSpec, LlamaServer, LocalClient, RuntimeError};
use serde_json::Value;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Restarts allowed after a crash before the model stays down.
const MAX_RESTARTS: u32 = 3;
/// A process that ran this long resets the restart budget.
const STABLE_AFTER: Duration = Duration::from_secs(600);

pub struct Hosted {
    pub model: Model,
    pub revision: String,
    pub profile: Profile,
    pub device: String,
    launch: LaunchSpec,
    ready: watch::Sender<Option<LocalClient>>,
    counter: Option<ClefCounter>,
    _in_use: File,
}

impl Hosted {
    pub fn new(
        model: Model,
        revision: String,
        profile: Profile,
        device: String,
        launch: LaunchSpec,
        counter: Option<ClefCounter>,
        in_use: File,
    ) -> Self {
        Self { model, revision, profile, device, launch, ready: watch::channel(None).0, counter, _in_use: in_use }
    }

    /// Client of the healthy runtime, or None while it is (re)starting.
    pub fn client(&self) -> Option<LocalClient> {
        self.ready.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<LocalClient>> {
        self.ready.subscribe()
    }

    pub fn loaded(&self) -> Option<LoadedModel> {
        self.client().map(|_| LoadedModel {
            model: self.model.id.clone(),
            model_revision: self.revision.clone(),
            profile: self.profile.id.clone(),
            device: self.device.clone(),
            context_tokens: self.profile.context_tokens,
        })
    }

    /// Exact input tokens of a payload for this model.
    pub async fn count(&self, payload: &Value) -> Result<u32, RuntimeError> {
        match &self.counter {
            Some(counter) => counter.count(payload).await,
            None => {
                let client = self.client().ok_or_else(|| {
                    RuntimeError::new(pool_protocol::messages::JobErrorCode::RuntimeFailed, "runtime is restarting")
                })?;
                client.chat_input_tokens(payload).await
            }
        }
    }

    /// Keeps the runtime alive until `shutdown`; `changes` is bumped whenever
    /// readiness changes so the connection resends Capacity.
    pub fn supervise(self: &Arc<Self>, changes: watch::Sender<u64>, shutdown: CancellationToken) -> JoinHandle<()> {
        let hosted = self.clone();
        tokio::spawn(async move {
            let label = hosted.model.id.clone();
            let mut restarts = 0;
            loop {
                let started = Instant::now();
                let server = tokio::select! {
                    server = LlamaServer::start(&hosted.launch) => server,
                    _ = shutdown.cancelled() => return,
                };
                match server {
                    Ok(mut server) => {
                        hosted.ready.send_replace(Some(server.client().clone()));
                        changes.send_modify(|n| *n += 1);
                        let reason = tokio::select! {
                            reason = server.wait_exit() => reason,
                            _ = shutdown.cancelled() => {
                                hosted.ready.send_replace(None);
                                server.stop();
                                let _ = tokio::time::timeout(Duration::from_secs(10), server.wait_exit()).await;
                                return;
                            }
                        };
                        hosted.ready.send_replace(None);
                        changes.send_modify(|n| *n += 1);
                        tracing::error!("{label}: llama-server stopped unexpectedly: {reason}");
                        if started.elapsed() > STABLE_AFTER {
                            restarts = 0;
                        }
                    }
                    Err(error) => tracing::error!("{label}: llama-server could not start: {error}"),
                }
                restarts += 1;
                if restarts > MAX_RESTARTS {
                    tracing::error!("{label}: giving up after {MAX_RESTARTS} restarts; the model stays unavailable");
                    return;
                }
                let wait = Duration::from_secs(5 * 3u64.pow(restarts - 1));
                tracing::warn!("{label}: restarting in {}s ({restarts}/{MAX_RESTARTS})", wait.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = shutdown.cancelled() => return,
                }
            }
        })
    }
}
