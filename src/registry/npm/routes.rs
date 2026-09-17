use axum::{
    routing::{get, put},
    Router,
};

use super::{
    delete_dist_tag, download_tarball, get_dist_tags, get_package, publish_package, put_dist_tag,
    search,
};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{repo}/@{scope}/{name}", get(get_package).put(publish_package))
        .route("/{repo}/@{scope}/{name}/-/{filename}", get(download_tarball))
        .route("/{repo}/{name}", get(get_package).put(publish_package))
        .route("/{repo}/{name}/-/{filename}", get(download_tarball))
        .route("/{repo}/-/v1/search", get(search))
        .route("/{repo}/-/package/@{scope}/{name}/dist-tags", get(get_dist_tags))
        .route(
            "/{repo}/-/package/@{scope}/{name}/dist-tags/{tag}",
            put(put_dist_tag).delete(delete_dist_tag),
        )
        .route("/{repo}/-/package/{name}/dist-tags", get(get_dist_tags))
        .route(
            "/{repo}/-/package/{name}/dist-tags/{tag}",
            put(put_dist_tag).delete(delete_dist_tag),
        )
}
