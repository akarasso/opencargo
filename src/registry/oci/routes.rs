use axum::{
    routing::{get, head, post, put},
    Router,
};

use super::api_version_check;
use super::blobs::{delete_blob, get_blob, head_blob};
use super::manifests::{delete_manifest, get_manifest, head_manifest, put_manifest};
use super::tags::list_tags;
use super::uploads::{complete_upload, start_upload, upload_chunk};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/", get(api_version_check))
        .route(
            "/v2/{repo}/{name}/blobs/{digest}",
            head(head_blob).get(get_blob).delete(delete_blob),
        )
        .route("/v2/{repo}/{name}/blobs/uploads/", post(start_upload))
        .route(
            "/v2/{repo}/{name}/blobs/uploads/{uuid}",
            put(complete_upload).patch(upload_chunk),
        )
        .route(
            "/v2/{repo}/{name}/manifests/{reference}",
            get(get_manifest)
                .head(head_manifest)
                .put(put_manifest)
                .delete(delete_manifest),
        )
        .route("/v2/{repo}/{name}/tags/list", get(list_tags))
}
