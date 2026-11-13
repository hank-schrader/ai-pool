use super::*;

/// Runs one request that fills most of the context, so the whole working set
/// is really allocated and used once before the profile is advertised. H2O first
/// verifies its label tokens and returns the engine the jobs will use.
pub(super) async fn probe(
    client: &LocalClient,
    backend: Backend,
    context: u32,
    slots: u32,
) -> Result<Option<H2o>, String> {
    // leaves room for the prompt template and the question around the filler
    let words = (context as usize * 85 / 100).saturating_sub(200);
    let filler = "please ".repeat(words);
    let request = async {
        match backend {
            Backend::LlamaServerSystemone | Backend::LlamaServerOpenjev => client
                .systemone(&json!({
                    "state": filler,
                    "questions": {"probe": {"type": "noul", "instructions": "Is this text repetitive?"}}
                }))
                .await
                .map(|_| None),
            Backend::LlamaServerChat => client
                .chat(&json!({"messages": [{"role": "user", "content": filler}], "max_tokens": 1}))
                .await
                .map(|_| None),
            Backend::LlamaServerH2o => {
                let h2o = H2o::start(client.clone(), context, slots).await?;
                h2o.answer(&json!({
                    "state": filler,
                    "questions": {"probe": {"type": "noul", "instructions": "Is this text repetitive?"}}
                }))
                .await
                .map(|_| Some(h2o))
            }
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, request).await {
        Ok(Ok(h2o)) => Ok(h2o),
        Ok(Err(error)) => Err(error.message),
        Err(_) => Err(format!("no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}
