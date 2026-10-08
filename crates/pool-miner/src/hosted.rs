//! One served model: its llama-server child kept alive by a supervisor, and,
//! for Clef, the token-count helper.
//!
//! A profile is only advertised after it answered a near-full-context probe.
//! Memory estimates cannot see live pressure (on a 16 GB Mac, Clef at 8192
//! loads but fails its first request), so an automatically chosen profile that
//! fails the probe steps down to the next smaller one. A profile forced with
//! `--profile`/`--context` is never changed silently; it stays unavailable.

use std::{
    fs::File,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pool_protocol::{Backend, Model, Profile, messages::LoadedModel};
use pool_runtime::{ClefCounter, LaunchSpec, LlamaServer, LocalClient, RuntimeError};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// Restarts allowed after a crash before the model stays down.
const MAX_RESTARTS: u32 = 3;
/// A process that ran this long resets the restart budget.
const STABLE_AFTER: Duration = Duration::from_secs(600);
/// A profile must answer a near-full request well inside the pool's
/// 150 s attempt limit, or it cannot serve requests of its own size.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// The runtime currently serving, and the profile it runs.
#[derive(Clone)]
pub struct Ready {
    pub client: LocalClient,
    pub profile: Profile,
}

pub struct Hosted {
    pub model: Model,
    pub revision: String,
    pub device: String,
    /// Profiles to try, the planned one first; smaller ones follow only when
    /// the plan chose automatically.
    profiles: Vec<Profile>,
    launch: LaunchSpec,
    ready: watch::Sender<Option<Ready>>,
    /// Base URL of a runtime a job found broken; the supervisor restarts it.
    broken: Mutex<Option<String>>,
    broken_notify: Notify,
    counter: Option<ClefCounter>,
    _in_use: File,
}

impl Hosted {
    pub fn new(
        model: Model,
        revision: String,
        profiles: Vec<Profile>,
        device: String,
        launch: LaunchSpec,
        counter: Option<ClefCounter>,
        in_use: File,
    ) -> Self {
        assert!(!profiles.is_empty(), "a hosted model needs a profile");
        Self {
            model,
            revision,
            device,
            profiles,
            launch,
            ready: watch::channel(None).0,
            broken: Mutex::new(None),
            broken_notify: Notify::new(),
            counter,
            _in_use: in_use,
        }
    }

    /// The healthy runtime and its profile, or None while it is (re)starting.
    pub fn ready(&self) -> Option<Ready> {
        self.ready.borrow().clone()
    }

    pub fn client(&self) -> Option<LocalClient> {
        self.ready().map(|ready| ready.client)
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Ready>> {
        self.ready.subscribe()
    }

    pub fn loaded(&self) -> Option<LoadedModel> {
        self.ready().map(|ready| LoadedModel {
            model: self.model.id.clone(),
            model_revision: self.revision.clone(),
            profile: ready.profile.id.clone(),
            device: self.device.clone(),
            context_tokens: ready.profile.context_tokens,
        })
    }

    /// Called when a job got a runtime failure from `client`: stop advertising
    /// it at once and have the supervisor restart and re-probe it. A Metal
    /// backend that hit out-of-memory stays broken until recreated.
    pub fn report_broken(&self, client: &LocalClient) {
        let current = self.ready.borrow().as_ref().is_some_and(|ready| ready.client.base_url() == client.base_url());
        if current {
            *self.broken.lock().unwrap() = Some(client.base_url().to_string());
            self.ready.send_replace(None);
            self.broken_notify.notify_one();
        }
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
            let mut level = 0;
            let mut restarts = 0;
            loop {
                let profile = hosted.profiles[level].clone();
                let mut launch = hosted.launch.clone();
                launch.context_tokens = profile.context_tokens;
                if let Some(layers) = profile.gpu_layers {
                    launch.gpu_layers = layers;
                }
                let started = Instant::now();
                let server = tokio::select! {
                    server = LlamaServer::start(&launch) => server,
                    _ = shutdown.cancelled() => return,
                };
                match server {
                    Ok(mut server) => {
                        let probed = tokio::select! {
                            probed = probe(server.client(), hosted.model.backend, profile.context_tokens) => probed,
                            _ = shutdown.cancelled() => {
                                server.stop();
                                let _ = tokio::time::timeout(Duration::from_secs(10), server.wait_exit()).await;
                                return;
                            }
                        };
                        if let Err(problem) = probed {
                            server.stop();
                            let _ = tokio::time::timeout(Duration::from_secs(10), server.wait_exit()).await;
                            if let Some(next) = hosted.profiles.get(level + 1) {
                                tracing::warn!(
                                    "{label}: {} failed its startup probe ({problem}); stepping down to {}",
                                    profile.id,
                                    next.id
                                );
                                level += 1;
                                continue;
                            }
                            let hint = if hosted.profiles.len() == 1 && level == 0 {
                                " (this profile was forced; try a smaller --context or --profile)"
                            } else {
                                ""
                            };
                            tracing::error!(
                                "{label}: {} failed its startup probe ({problem}) and no smaller profile is left{hint}; \
                                 the model stays unavailable",
                                profile.id
                            );
                            return;
                        }
                        tracing::info!(
                            "{label}: {} passed its {}-token probe in {:.1}s",
                            profile.id,
                            profile.context_tokens,
                            started.elapsed().as_secs_f64()
                        );
                        hosted.ready.send_replace(Some(Ready { client: server.client().clone(), profile }));
                        changes.send_modify(|n| *n += 1);
                        let reason = loop {
                            tokio::select! {
                                reason = server.wait_exit() => break reason,
                                _ = hosted.broken_notify.notified() => {
                                    let url = server.client().base_url().to_string();
                                    if hosted.broken.lock().unwrap().as_deref() == Some(url.as_str()) {
                                        server.stop();
                                        let _ = tokio::time::timeout(Duration::from_secs(10), server.wait_exit()).await;
                                        break "a job hit a runtime failure".to_string();
                                    }
                                }
                                _ = shutdown.cancelled() => {
                                    hosted.ready.send_replace(None);
                                    server.stop();
                                    let _ = tokio::time::timeout(Duration::from_secs(10), server.wait_exit()).await;
                                    return;
                                }
                            }
                        };
                        hosted.ready.send_replace(None);
                        changes.send_modify(|n| *n += 1);
                        tracing::error!("{label}: llama-server stopped: {reason}; it is restarted and probed again");
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

/// Runs one request that fills most of the context, so the whole working set
/// is really allocated and used once before the profile is advertised.
async fn probe(client: &LocalClient, backend: Backend, context: u32) -> Result<(), String> {
    // leaves room for the prompt template and the question around the filler
    let words = (context as usize * 85 / 100).saturating_sub(200);
    let filler = "please ".repeat(words);
    let request = async {
        match backend {
            Backend::LlamaServerSystemone => client
                .systemone(&json!({
                    "state": filler,
                    "questions": {"probe": {"type": "noul", "instructions": "Is this text repetitive?"}}
                }))
                .await
                .map(drop),
            Backend::LlamaServerChat => client
                .chat(&json!({"messages": [{"role": "user", "content": filler}], "max_tokens": 1}))
                .await
                .map(drop),
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, request).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.message),
        Err(_) => Err(format!("no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}
