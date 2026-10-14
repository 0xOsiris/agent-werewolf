//! Static files from `crates/server/static/`, embedded in release builds.

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

/// Serves `path` from the embedded directory, or 404.
pub fn serve(path: &str) -> Response {
    if !is_safe(path) {
        return HttpError::NotFound.into_response();
    }
    let Some(file) = Assets::get(path) else {
        return HttpError::NotFound.into_response();
    };
    let headers = [
        (CONTENT_TYPE, file.metadata.mimetype().to_owned()),
        // The frontend changes between demo runs; always revalidate.
        (CACHE_CONTROL, "no-cache".to_owned()),
        (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
    ];
    (headers, file.data).into_response()
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
}
