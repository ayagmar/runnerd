//! The files the pages load, compiled into the binary: the stylesheet, the theme switch, the vendored Datastar
//! bundle, the font, the mark, and the web app manifest with its icons, which let a browser install the dashboard. Nothing is fetched from elsewhere; `assets/README.md` records where each vendored
//! file came from and its SHA-256.

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

const FILES: [(&str, &str, &[u8]); 9] = [
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
    (
        "manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../assets/manifest.webmanifest"),
    ),
    (
        "icon-192.png",
        "image/png",
        include_bytes!("../assets/icon-192.png"),
    ),
    (
        "icon-512.png",
        "image/png",
        include_bytes!("../assets/icon-512.png"),
    ),
    (
        "icon-maskable-512.png",
        "image/png",
        include_bytes!("../assets/icon-maskable-512.png"),
    ),
];

/// An asset's version: a digest of its bytes (64-bit FNV-1a, in hex), so its URL changes whenever it does.
fn version(body: &[u8]) -> String {
    let digest = body.iter().fold(0xcbf2_9ce4_8422_2325_u64, |digest, byte| {
        (digest ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{digest:016x}")
}

fn file(name: &str) -> Option<&'static (&'static str, &'static str, &'static [u8])> {
    FILES.iter().find(|(file, _, _)| *file == name)
}

/// The URL of an asset, versioned by its content, so the browser keeps it until it changes. The stylesheet names
/// the font by this URL too.
pub(crate) fn path(name: &str) -> String {
    let version = file(name)
        .map(|(_, _, body)| version(body))
        .unwrap_or_default();
    format!("/assets/{name}?v={version}")
}

/// `?v=` of an asset's URL.
#[derive(Debug, Deserialize)]
pub(crate) struct Version {
    v: Option<String>,
}

/// `GET /assets/{name}`. A request for the current version may be cached for good; any other is revalidated by
/// its `ETag`, the version, each time.
pub(crate) async fn serve(
    Path(name): Path<String>,
    Query(asked): Query<Version>,
    headers: HeaderMap,
) -> Response {
    let Some((_, content_type, body)) = file(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let version = version(body);
    let cache = if asked.v.as_deref() == Some(version.as_str()) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let etag = format!("\"{version}\"");
    let headers_out = [
        (header::CONTENT_TYPE, (*content_type).to_owned()),
        (header::CACHE_CONTROL, cache.to_owned()),
        (header::ETAG, etag.clone()),
    ];
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .any(|tag| tag.trim() == etag || tag.trim() == "*")
        });
    if unchanged {
        return (StatusCode::NOT_MODIFIED, headers_out).into_response();
    }
    (headers_out, *body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stylesheet_loads_the_font_from_the_url_the_page_preloads() {
        let css = std::str::from_utf8(include_bytes!("../assets/style.css")).unwrap_or_default();
        let font = format!("url('{}')", path("libre-franklin.woff2"));
        assert!(css.contains(&font), "style.css names the font as {font}");
    }
}
