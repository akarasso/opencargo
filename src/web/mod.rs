use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use rust_embed::Embed;

use crate::server::AppState;

// ---------------------------------------------------------------------------
// Embedded frontend assets (built SolidJS SPA from frontend/dist/)
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "frontend/dist/"]
struct FrontendAssets;

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Serve static assets from the embedded frontend build (JS, CSS, etc.)
async fn serve_asset(Path(path): Path<String>) -> impl IntoResponse {
    let asset_path = format!("assets/{}", path);
    match FrontendAssets::get(&asset_path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (
                        header::CACHE_CONTROL,
                        "public, max-age=31536000, immutable".to_string(),
                    ),
                ],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// SPA fallback: serve index.html for all non-API, non-asset routes.
async fn serve_spa() -> impl IntoResponse {
    match FrontendAssets::get("index.html") {
        Some(content) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
            content.data.to_vec(),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Frontend not built. Run: cd frontend && pnpm build",
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn web_routes() -> Router<AppState> {
    Router::new()
        .route("/assets/{*path}", get(serve_asset))
        // Explicit SPA routes to prevent conflicts with npm protocol routes
        // (e.g. /{repo}/@{scope}/{name} would match /packages/@scope/name)
        .route("/", get(serve_spa))
        .route("/packages", get(serve_spa))
        .route("/packages/{*path}", get(serve_spa))
        .route("/search", get(serve_spa))
        .route("/oci", get(serve_spa))
        .route("/go", get(serve_spa))
        .route("/login", get(serve_spa))
        .route("/admin", get(serve_spa))
        .route("/admin/repositories", get(serve_spa))
        .route("/admin/users", get(serve_spa))
        .route("/admin/users/{username}/tokens", get(serve_spa))
        .route("/admin/packages", get(serve_spa))
        .route("/admin/audit", get(serve_spa))
        .route("/admin/system", get(serve_spa))
        .route("/admin/password", get(serve_spa))
        .route("/admin/webhooks", get(serve_spa))
        .fallback(get(serve_spa))
}
