use axum::http::Request;

/// Pre-route hook for nested image names (`team/app`); until they land every
/// `/v2` route binds `{name}` to one segment, so the request passes untouched.
pub fn rewrite_nested_name<B>(req: Request<B>) -> Request<B> {
    req
}
