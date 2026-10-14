//! Exact lookup of the committed, embedded production assets.
use axum::{
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'";

pub async fn index() -> Response {
    serve("index.html")
}

pub async fn asset(Path(path): Path<String>) -> Response {
    serve(&format!("assets/{path}"))
}

fn serve(path: &str) -> Response {
    let Some(bytes) = lookup(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    let html = path == "index.html";
    let mut response = (
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, if html { "no-cache" } else { "public, max-age=31536000, immutable" }),
        ],
        Body::from(bytes),
    )
        .into_response();
    if html {
        response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, CSP.parse().unwrap());
    }
    response
}
