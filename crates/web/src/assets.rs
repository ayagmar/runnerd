//! The files the pages load, compiled into the binary: the stylesheet, the theme switch, the vendored Datastar
//! bundle, the font and the mark. Nothing is fetched from elsewhere; `assets/README.md` records where each vendored
//! file came from and its SHA-256.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const FILES: [(&str, &str, &[u8]); 5] = [
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/style.css"),
    ),
    (
        "theme.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/theme.js"),
    ),
    (
        "datastar.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/datastar.js"),
    ),
    (
        "libre-franklin.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/libre-franklin-latin-wght-normal.woff2"),
    ),
    (
        "mark.svg",
        "image/svg+xml",
        include_bytes!("../assets/mark.svg"),
    ),
];

/// The URL of an asset, versioned so a new release is not served from the browser's cache.
pub(crate) fn path(name: &str) -> String {
    format!("/assets/{name}?v={}", env!("CARGO_PKG_VERSION"))
}

/// `GET /assets/{name}`.
pub(crate) async fn serve(Path(name): Path<String>) -> Response {
    match FILES.iter().find(|(file, _, _)| *file == name) {
        Some((_, content_type, body)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
