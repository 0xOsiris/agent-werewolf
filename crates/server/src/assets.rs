//! Static files from `crates/server/static/`, embedded in release builds.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::LazyLock,
};

use axum::{
    http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::error::HttpError;

/// Debug builds read from disk, so frontend edits show up without a rebuild.
#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

const NO_CACHE: &str = "no-cache";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Changes whenever any asset does. Pages link assets under it because Cloudflare overrides
/// `no-cache` with a 4-hour browser TTL, which left players running stale scripts after a deploy.
static VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut paths: Vec<_> = Assets::iter().collect();
    paths.sort();
    let mut hasher = DefaultHasher::new();
    for path in paths {
        path.hash(&mut hasher);
        if let Some(file) = Assets::get(&path) {
            file.metadata.sha256_hash().hash(&mut hasher);
        }
    }
    format!("{:016x}", hasher.finish())
});

/// Serves an HTML page with its `/static/` links pinned to this build's version.
pub fn page(name: &str) -> Response {
    let Some(file) = Assets::get(name) else {
        return HttpError::NotFound.into_response();
    };
    let html = String::from_utf8_lossy(&file.data)
        .replace("\"/static/", &format!("\"/static/{}/", *VERSION));
    let headers = [
        (CONTENT_TYPE, file.metadata.mimetype().to_owned()),
        (CACHE_CONTROL, NO_CACHE.to_owned()),
        (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
    ];
    (headers, html).into_response()
}

/// Serves `path` from the embedded directory, or 404.
pub fn serve(path: &str) -> Response {
    let (path, cache) = unversion(path, &VERSION);
    if !is_safe(path) {
        return HttpError::NotFound.into_response();
    }
    let Some(file) = Assets::get(path) else {
        return HttpError::NotFound.into_response();
    };
    // Debug builds serve edited files under the same version, so they must revalidate.
    let cache = if cfg!(debug_assertions) {
        NO_CACHE
    } else {
        cache
    };
    let headers = [
        (CONTENT_TYPE, file.metadata.mimetype().to_owned()),
        (CACHE_CONTROL, cache.to_owned()),
        (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
    ];
    (headers, file.data).into_response()
}

/// Strips a version prefix: this build's is cached forever; an old page's still gets today's file.
fn unversion<'a>(path: &'a str, version: &str) -> (&'a str, &'static str) {
    match path.split_once('/') {
        Some((v, rest)) if v == version => (rest, IMMUTABLE),
        Some((v, rest)) if v.len() == 16 && v.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (rest, NO_CACHE)
        }
        _ => (path, NO_CACHE),
    }
}

/// Plain relative paths only: no traversal, hidden files, or odd characters.
fn is_safe(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_hidden_files() {
        for bad in [
            "",
            "../Cargo.toml",
            "a/../../x",
            "/etc/passwd",
            ".env",
            "a//b",
            "a\\b",
            "x%2e",
        ] {
            assert!(!is_safe(bad), "{bad:?}");
        }
        for good in ["index.html", "sprites/wolf-1.png", "app.min.js"] {
            assert!(is_safe(good), "{good:?}");
        }
    }

    #[test]
    fn versioned_paths_cache_forever_and_old_versions_still_load() {
        let v = "0123456789abcdef";
        assert_eq!(
            unversion("0123456789abcdef/js/play.js", v),
            ("js/play.js", IMMUTABLE)
        );
        assert_eq!(
            unversion("fedcba9876543210/js/play.js", v),
            ("js/play.js", NO_CACHE)
        );
        assert_eq!(unversion("js/play.js", v), ("js/play.js", NO_CACHE));
        assert_eq!(unversion("../js/play.js", v), ("../js/play.js", NO_CACHE));
    }

    #[tokio::test]
    async fn pages_link_assets_under_the_version() {
        let body = axum::body::to_bytes(page("play.html").into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains(&format!("src=\"/static/{}/js/play.js\"", *VERSION)));
        assert!(
            !html.contains("\"/static/js/"),
            "every asset link is versioned"
        );
        let asset = serve(&format!("{}/js/play.js", *VERSION));
        assert_eq!(asset.status(), axum::http::StatusCode::OK);
    }
}
