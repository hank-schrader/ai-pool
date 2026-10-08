//! `pool-miner selftest`: start the selected models exactly as `run` would,
//! then count and run one request per model (plus a chat stream), no pool.

use std::time::{Duration, Instant};

use pool_protocol::{Backend, Operation, requests};
use pool_runtime::client::StreamItem;
use serde_json::{Value, json};

use crate::{Cli, hardware, setup};

pub async fn run(cli: &Cli) -> Result<(), String> {
    let (catalog, revision) = cli.load_catalog(false).await?;
    let machine = hardware::detect(cli.device)?;
    eprintln!("Accelerator: {}", machine.describe());
    eprintln!("Catalog revision: {revision}");
    let selected = cli.select(&catalog, &machine)?;
    let options = cli.options()?;
    let running = setup::start(&catalog, &selected, &machine, &options).await?;
    let result = exercise(&running).await;
    running.stop().await;
    result
}

async fn exercise(running: &setup::Running) -> Result<(), String> {
    running.wait_ready(Duration::from_secs(600)).await?;
    let mut failures = 0;
    for hosted in &running.models {
        let model = &hosted.model;
        let ready = hosted.ready().ok_or("runtime not ready")?;
        let client = ready.client;
        println!("== {} ({}, context {})", model.id, ready.profile.id, ready.profile.context_tokens);
        let (operation, body) = match model.backend {
            Backend::LlamaServerSystemone => (
                Operation::Systemone,
                json!({
                    "model": model.id,
                    "state": "Customer message: I was charged twice for my order last week and nobody has replied.",
                    "questions": {
                        "route": {"type": "choice", "instructions": "Which team should handle this?",
                                  "criteria": {"billing": null, "shipping": null, "technical": null}},
                        "angry": {"type": "noul", "instructions": "Is the customer angry?"}
                    }
                }),
            ),
            Backend::LlamaServerChat => (
                Operation::ChatCompletions,
                json!({"model": model.id, "messages": [{"role": "user", "content": "Say hello in five words."}], "max_tokens": 24}),
            ),
        };
        let validated = requests::validate(operation, model, body.as_object().unwrap().clone())
            .map_err(|error| format!("{}: {}", error.code, error.message))?;

        let started = Instant::now();
        let counted = hosted.count(&validated.count_payload).await;
        println!("count: {counted:?} in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);

        let started = Instant::now();
        let reply = match operation {
            Operation::Systemone => client.systemone(&validated.payload).await,
            Operation::ChatCompletions => client.chat(&validated.payload).await,
        };
        match &reply {
            Ok(body) => {
                let usage = body.get("usage").cloned().unwrap_or(Value::Null);
                let summary = match operation {
                    Operation::Systemone => body["answers"].to_string(),
                    Operation::ChatCompletions => {
                        body.pointer("/choices/0/message/content").cloned().unwrap_or_default().to_string()
                    }
                };
                println!("result in {:.0} ms: {summary}", started.elapsed().as_secs_f64() * 1000.0);
                println!("usage: {usage}");
                let reported = usage.get("input_tokens").or(usage.get("prompt_tokens")).and_then(Value::as_u64);
                if operation == Operation::Systemone && counted.as_ref().ok().map(|n| *n as u64) != reported {
                    failures += 1;
                    println!("MISMATCH: counted {counted:?}, runtime reported {reported:?}");
                }
            }
            Err(error) => {
                failures += 1;
                println!("inference failed: {error}");
            }
        }

        if operation == Operation::ChatCompletions {
            let mut payload = validated.payload.clone();
            payload["stream"] = true.into();
            payload["stream_options"] = json!({"include_usage": true});
            let started = Instant::now();
            let mut stream = client.chat_stream(&payload).await.map_err(|error| error.to_string())?;
            let (mut chunks, mut text, mut usage, mut done) = (0, String::new(), None, false);
            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamItem::Chunk(chunk)) => {
                        if chunk["choices"].as_array().is_some_and(Vec::is_empty) {
                            usage = chunk.get("usage").cloned();
                        } else {
                            chunks += 1;
                            text.push_str(
                                chunk.pointer("/choices/0/delta/content").and_then(Value::as_str).unwrap_or(""),
                            );
                        }
                    }
                    Ok(StreamItem::Done) => {
                        done = true;
                        break;
                    }
                    Err(error) => {
                        println!("stream failed: {error}");
                        break;
                    }
                }
            }
            println!(
                "stream: {chunks} chunks, [DONE]={done}, {:.0} ms: {text:?}",
                started.elapsed().as_secs_f64() * 1000.0
            );
            println!("stream usage: {}", usage.unwrap_or(Value::Null));
            if !done {
                failures += 1;
            }
        }
    }
    if failures > 0 { Err(format!("{failures} check(s) failed")) } else { Ok(()) }
}
