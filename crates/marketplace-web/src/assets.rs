//! Assets shared by the preview server and each static publication.
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(crate) struct Asset {
    pub name: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! asset {
    ($name:literal, $content_type:literal) => {
        Asset {
            name: $name,
            content_type: $content_type,
            bytes: include_bytes!(concat!("../static/assets/", $name)),
        }
    };
}

pub(crate) const ASSETS: &[Asset] = &[
    asset!("marketplace.css", "text/css; charset=utf-8"),
    asset!("sandstorm-logo.svg", "image/svg+xml"),
    asset!("welcome-banner.png", "image/png"),
    asset!("favicon.png", "image/png"),
    asset!("sourcesans-light.woff2", "font/woff2"),
    asset!("sourcesans-regular.woff2", "font/woff2"),
    asset!("sourcesans-semibold.woff2", "font/woff2"),
    asset!("Sandstorm-LICENSE.txt", "text/plain; charset=utf-8"),
    asset!("SourceSans-LICENSE.txt", "text/plain; charset=utf-8"),
];

pub(crate) async fn serve(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|asset| asset.name == name) {
        Some(asset) => ([(header::CONTENT_TYPE, asset.content_type)], asset.bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
