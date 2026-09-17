use std::collections::BTreeMap;
use std::str::FromStr;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::policy::store::{self, ReportFilter, Subject};
use crate::policy::Age;
use crate::server::AppState;

use super::{record_audit, require_admin, require_auth};

#[derive(Deserialize)]
pub struct ReportQuery {
    since: Option<String>,
    repo: Option<String>,
    rule: Option<String>,
    page: Option<i64>,
    size: Option<i64>,
}

#[derive(Deserialize)]
pub struct EraseQuery {
    user: Option<String>,
    user_id: Option<i64>,
}

/// An `Age` ("24h", "7d") back from `now`, or an RFC 3339 instant; 24h by default.
fn parse_since(s: Option<&str>, now: DateTime<Utc>) -> AppResult<DateTime<Utc>> {
    let Some(s) = s else {
        return Ok(now - chrono::Duration::hours(24));
    };
    if let Ok(age) = Age::from_str(s) {
        return chrono::Duration::from_std(age.duration())
            .ok()
            .and_then(|delta| now.checked_sub_signed(delta))
            .ok_or_else(|| AppError::BadRequest(format!("invalid since '{s}': age out of range")));
    }
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| {
            AppError::BadRequest(format!(
                "invalid since '{s}': expected an age (24h, 7d) or an RFC 3339 instant"
            ))
        })
}

fn check_rule<'a>(state: &AppState, rule: Option<&'a str>) -> AppResult<Option<&'a str>> {
    let names = state.policy.rule_names();
    match rule {
        Some(r) if !names.contains(&r) => Err(AppError::BadRequest(format!(
            "unknown rule '{r}': expected one of {}",
            names.join(", ")
        ))),
        other => Ok(other),
    }
}

/// GET /api/v1/policy/report (admin)
pub async fn report(
    State(state): State<AppState>,
    Query(q): Query<ReportQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    render(&state, &q, None).await
}

/// GET /api/v1/me/policy: the caller's own rows, whatever the query says.
pub async fn me_policy(
    State(state): State<AppState>,
    Query(q): Query<ReportQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    let subject = match caller.user_id {
        Some(id) => Subject::User(id),
        None => Subject::Static,
    };
    render(&state, &q, Some(subject)).await
}

async fn render(
    state: &AppState,
    q: &ReportQuery,
    subject: Option<Subject>,
) -> AppResult<Json<Value>> {
    let since = parse_since(q.since.as_deref(), Utc::now())?;
    let rule = check_rule(state, q.rule.as_deref())?;
    let admin = subject.is_none();
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(50).clamp(1, 200);
    let filter = ReportFilter {
        since,
        repo: q.repo.as_deref(),
        rule,
        subject,
    };
    let key = format!(
        "{}|{}|{}|{subject:?}",
        q.since.as_deref().unwrap_or(""),
        q.repo.as_deref().unwrap_or(""),
        rule.unwrap_or("")
    );
    let totals = state.policy.totals(&filter, key).await?;
    let rows = store::list_resolutions(&state.db, &filter, page, size).await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut verdicts: BTreeMap<i64, Vec<store::VerdictRow>> = BTreeMap::new();
    for v in store::verdicts_for(&state.db, &ids, rule).await? {
        verdicts.entry(v.resolution_id).or_default().push(v);
    }
    let entries: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mut entry = serde_json::to_value(&r).unwrap_or_default();
            entry["verdicts"] = json!(verdicts.remove(&r.id).unwrap_or_default());
            entry
        })
        .collect();
    let mut body = json!({
        "since": since.to_rfc3339_opts(SecondsFormat::Secs, true),
        "page": page,
        "size": size,
        "totals": totals,
        "entries": entries,
    });
    if admin {
        body["process"] = json!({ "dropped_since_start": state.policy.dropped() });
    }
    Ok(Json(body))
}

/// DELETE /api/v1/policy/report?user=|user_id= (admin): erases one user's
/// rows and audits the count, never the name.
pub async fn erase(
    State(state): State<AppState>,
    Query(q): Query<EraseQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let user_id = match (q.user.as_deref(), q.user_id) {
        (Some(name), None) => crate::db::get_user_by_username(&state.db, name)
            .await?
            .map(|u| u.id)
            .ok_or_else(|| AppError::NotFound(format!("user not found: {name}")))?,
        (None, Some(id)) => id,
        _ => {
            return Err(AppError::BadRequest(
                "exactly one of user or user_id is required".to_string(),
            ))
        }
    };
    let deleted = store::delete_by_user(&state.db, user_id).await?;
    state.policy.forget_totals();
    record_audit(
        &state,
        &caller,
        "policy.erase",
        Some(&format!("deleted={deleted}")),
    )
    .await;
    Ok(Json(json!({ "deleted": deleted })))
}

/// GET /api/v1/policy/rules (admin): the effective config of every proxy
/// repository, defaults included, and which members record.
pub async fn rules(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let mut recording = Vec::new();
    let mut repositories = serde_json::Map::new();
    for repo in crate::db::get_all_repositories(&state.db).await? {
        if repo.repo_type != "proxy" {
            continue;
        }
        if state.policy.records(&repo.name) {
            recording.push(repo.name.clone());
        }
        let cfg = state.policy.config_for(&repo.name);
        repositories.insert(
            repo.name,
            json!({
                "min_release_age": cfg.min_release_age.map(|a| a.to_string()),
                "osv_severity": cfg.osv_severity.map(|s| s.as_str()),
                "install_scripts": cfg.install_scripts,
                "typosquat": cfg.typosquat,
                "fetch_missing_facts": cfg.fetch_missing_facts,
            }),
        );
    }
    Ok(Json(json!({
        "osv_enabled": state.vuln_scan_config.enabled,
        "recording": recording,
        "repositories": repositories,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn since_accepts_age_and_rfc3339_defaults_to_24h() {
        let day = chrono::Duration::hours(24);
        assert_eq!(parse_since(None, now()).unwrap(), now() - day);
        assert_eq!(parse_since(Some("7d"), now()).unwrap(), now() - day * 7);
        assert_eq!(
            parse_since(Some("2026-09-16T00:00:00Z"), now()).unwrap(),
            DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z").unwrap()
        );
        assert_eq!(
            parse_since(Some("2026-09-16T02:00:00+02:00"), now()).unwrap(),
            DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn since_rejects_words_and_weeks() {
        for bad in ["1w", "7 days", "yesterday", ""] {
            let err = parse_since(Some(bad), now()).unwrap_err().to_string();
            assert!(err.contains("invalid since"), "{bad}: {err}");
        }
    }

    #[test]
    fn since_rejects_ages_before_the_epoch_of_time() {
        for huge in [
            "9999999999d",
            "18446744073709551615s",
            "5124095576030h",
            "307445734561825m",
        ] {
            assert!(Age::from_str(huge).is_ok(), "{huge} is a valid Age");
            let err = parse_since(Some(huge), now()).unwrap_err().to_string();
            assert!(err.contains("age out of range"), "{huge}: {err}");
        }
        assert!(parse_since(Some("100000d"), now()).is_ok());
    }
}
