use super::*;
use crate::domain::Format;
use crate::policy::testing::{scanner, FakeOsv};
use crate::policy::{Actor, Facts};

const V3_CRITICAL: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";
const V3_MEDIUM: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N";

fn resolution(format: Format, name: &str, version: Option<&str>) -> Resolution {
    Resolution {
        requested_repo: "r".into(),
        member_repo: "m".into(),
        format,
        name: name.into(),
        version: version.map(String::from),
        digest: None,
        actor: Actor::of(None),
        published_at: None,
        facts: Facts::default(),
    }
}

fn cfg(level: Severity) -> PolicyConfig {
    PolicyConfig {
        osv_severity: Some(level),
        ..Default::default()
    }
}

fn rule(osv: Option<&FakeOsv>) -> (OsvSeverity, Arc<OsvMemo>) {
    let memo = new_memo();
    (OsvSeverity::new(scanner(osv), memo.clone()), memo)
}

fn finding(top: Option<(&str, Severity)>, at: DateTime<Utc>) -> OsvFinding {
    OsvFinding {
        top: top.map(|(id, sev)| (id.to_string(), sev)),
        at,
    }
}

#[test]
fn oci_not_applicable() {
    let (rule, _) = rule(None);
    let v = rule
        .evaluate(
            &cfg(Severity::High),
            &resolution(Format::Oci, "nginx", Some("1")),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(v.verdict, Verdict::NotApplicable);
    assert_eq!(v.reason, "oci: no osv ecosystem");
    assert!(rule
        .evaluate(
            &PolicyConfig::default(),
            &resolution(Format::Npm, "lodash", Some("1")),
            Utc::now()
        )
        .is_none());
}

#[test]
fn disabled_scanner_is_unknown() {
    let (rule, _) = rule(None);
    let v = rule
        .evaluate(
            &cfg(Severity::High),
            &resolution(Format::Npm, "lodash", Some("1")),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        (v.verdict, v.reason.as_str()),
        (Verdict::Unknown, "osv scanning disabled")
    );
}

#[tokio::test]
async fn unresolved_version_is_unknown() {
    let osv = FakeOsv::start().await;
    let (rule, _) = rule(Some(&osv));
    let v = rule
        .evaluate(
            &cfg(Severity::High),
            &resolution(Format::Npm, "lodash", None),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        (v.verdict, v.reason.as_str()),
        (Verdict::Unknown, "version unresolved")
    );
}

#[test]
fn threshold_is_inclusive() {
    let now = Utc::now();
    let high = finding(Some(("GHSA-x", Severity::High)), now);
    let v = verdict(Severity::High, &high);
    assert_eq!(
        (v.verdict, v.reason.as_str()),
        (Verdict::WouldBlock, "GHSA-x high >= high")
    );
    let v = verdict(Severity::Critical, &high);
    assert_eq!(
        (v.verdict, v.reason.as_str()),
        (Verdict::Pass, "GHSA-x high < critical")
    );
    let unknown = finding(Some(("GHSA-u", Severity::Unknown)), now);
    let v = verdict(Severity::Low, &unknown);
    assert_eq!(v.verdict, Verdict::Pass);
    assert!(v.reason.contains("no severity"), "{}", v.reason);
    let clean = verdict(Severity::Low, &finding(None, now));
    assert_eq!(
        (clean.verdict, clean.reason.as_str()),
        (Verdict::Pass, "no known vulnerability")
    );
}

#[tokio::test]
async fn memo_hit_is_not_deferred() {
    let osv = FakeOsv::start().await;
    let (rule, memo) = rule(Some(&osv));
    let now = Utc::now();
    let key = (Format::Npm, "lodash".to_string(), "4.17.21".to_string());
    memo.lock()
        .unwrap()
        .insert(key.clone(), finding(Some(("GHSA-x", Severity::High)), now));
    let r = resolution(Format::Npm, "lodash", Some("4.17.21"));
    let v = rule.evaluate(&cfg(Severity::High), &r, now).unwrap();
    assert_eq!(v.verdict, Verdict::WouldBlock);
    let v = rule.evaluate(&cfg(Severity::Critical), &r, now).unwrap();
    assert_eq!(v.verdict, Verdict::Pass);
    memo.lock().unwrap().insert(key.clone(), finding(None, now));
    assert_eq!(
        rule.evaluate(&cfg(Severity::Low), &r, now).unwrap().verdict,
        Verdict::Pass
    );
    let later = now + chrono::Duration::hours(2);
    assert!(
        rule.evaluate(&cfg(Severity::Low), &r, later).is_none(),
        "expired: deferred"
    );
    assert!(rule
        .evaluate(
            &cfg(Severity::Low),
            &resolution(Format::Npm, "lodash", Some("4.17.20")),
            now
        )
        .is_none());
    assert!(osv.batches().is_empty(), "evaluate never queries");
}

fn batch_rows(osv: &FakeOsv) -> (Arc<dyn VulnFeed>, Vec<Row>) {
    let mut rows: Vec<Row> = (0..64)
        .map(|i| {
            (
                resolution(Format::Npm, "lodash", Some(&format!("1.0.{}", i % 3))),
                vec![None],
            )
        })
        .collect();
    for i in 0..3 {
        rows.push((
            resolution(Format::Cargo, "serde", Some(&format!("1.0.{i}"))),
            vec![None],
        ));
    }
    (scanner(Some(osv)), rows)
}

#[tokio::test]
async fn batch_groups_by_ecosystem_and_dedupes() {
    let osv = FakeOsv::start().await;
    osv.affect("npm", "lodash", "1.0.0", "GHSA-a", V3_CRITICAL);
    osv.affect("crates.io", "serde", "1.0.2", "GHSA-b", V3_MEDIUM);
    let (scanner, mut rows) = batch_rows(&osv);
    let memo = new_memo();
    evaluate_batch(scanner.as_ref(), &memo, |_| Some(Severity::High), &mut rows).await;
    let mut batches = osv.batches();
    batches.sort();
    assert_eq!(batches, [3, 3], "one POST per ecosystem, deduplicated");
    for (r, verdicts) in &rows {
        let v = verdicts[0].as_ref().expect("filled at flush");
        match (r.format, r.version.as_deref()) {
            (Format::Npm, Some("1.0.0")) => {
                assert_eq!(
                    (v.verdict, v.reason.as_str()),
                    (Verdict::WouldBlock, "GHSA-a critical >= high")
                );
            }
            (Format::Cargo, Some("1.0.2")) => {
                assert_eq!(
                    (v.verdict, v.reason.as_str()),
                    (Verdict::Pass, "GHSA-b medium < high")
                );
            }
            _ => assert_eq!(
                (v.verdict, v.reason.as_str()),
                (Verdict::Pass, "no known vulnerability")
            ),
        }
    }
    let now = Utc::now();
    let top = |format: Format, name: &str, version: &str| {
        memo_hit(&memo, &(format, name.into(), version.into()), now)
            .expect("memoised")
            .top
    };
    assert_eq!(
        top(Format::Npm, "lodash", "1.0.0"),
        Some(("GHSA-a".into(), Severity::Critical))
    );
    assert_eq!(top(Format::Npm, "lodash", "1.0.1"), None);
    assert_eq!(
        top(Format::Cargo, "serde", "1.0.2"),
        Some(("GHSA-b".into(), Severity::Medium))
    );
    let (_, mut again) = batch_rows(&osv);
    evaluate_batch(scanner.as_ref(), &memo, |_| Some(Severity::High), &mut again).await;
    assert_eq!(osv.batches().len(), 2, "the memo answers the next flush");
    assert!(again.iter().all(|(_, v)| v[0].is_some()));
}

#[tokio::test]
async fn batch_error_marks_every_row_unknown() {
    let osv = FakeOsv::start().await;
    osv.set_down(true);
    let (scanner, mut rows) = batch_rows(&osv);
    let memo = new_memo();
    evaluate_batch(scanner.as_ref(), &memo, |_| Some(Severity::High), &mut rows).await;
    assert_eq!(osv.batches().len(), 2);
    for (_, verdicts) in &rows {
        let v = verdicts[0].as_ref().unwrap();
        assert_eq!(v.verdict, Verdict::Unknown);
        assert!(v.reason.starts_with("osv unreachable: "), "{}", v.reason);
    }
    let now = Utc::now();
    assert!(memo_hit(&memo, &(Format::Npm, "lodash".into(), "1.0.0".into()), now).is_none());
}
