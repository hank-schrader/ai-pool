//! The `llama-server-h2o` backend: refactor-tool's H2O-Lightning decision
//! engine (default policy `single`) served through llama-server's
//! `/tokenize` and `/completion`. Each question is rendered, tokenized and
//! answered with one label-only forward pass; see [`engine::H2o`].

mod engine;
mod float;
mod json;
mod prompt;

pub use engine::H2o;

#[cfg(test)]
mod tests;
