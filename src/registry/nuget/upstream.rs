use crate::domain::{CacheRepo, Outcome};
use crate::proxy::Payload;
use crate::registry::resolve::{Cx, ResolveError, Upstream};

use super::leaves::Coordinates;
use super::model::Entry;

fn unserved() -> ResolveError {
    ResolveError::Upstream("nuget proxy repositories are not served yet".to_string())
}

pub async fn entries(
    _cx: &Cx<'_>,
    _member: CacheRepo<'_>,
    _up: &Upstream,
    _id: &str,
) -> Result<Outcome<Vec<Entry>>, ResolveError> {
    Err(unserved())
}

pub async fn nupkg(
    _cx: &Cx<'_>,
    _member: CacheRepo<'_>,
    _up: &Upstream,
    _at: &Coordinates,
) -> Result<Outcome<Payload>, ResolveError> {
    Err(unserved())
}

pub async fn nuspec(
    _cx: &Cx<'_>,
    _member: CacheRepo<'_>,
    _up: &Upstream,
    _at: &Coordinates,
) -> Result<Outcome<Payload>, ResolveError> {
    Err(unserved())
}
