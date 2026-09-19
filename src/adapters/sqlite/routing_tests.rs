use super::*;
use crate::testing::fixture::Fx;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-19T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

async fn store(fx: &Fx) -> SqliteRoutingRuleStore {
    let pool = crate::adapters::sqlite::connect(&format!("sqlite:{}", fx.db_path().display()))
        .await
        .unwrap();
    SqliteRoutingRuleStore::new(pool)
}

fn rule<'a>(name: &'a str, patterns: &'a [String], effect: &'a Effect) -> NewRule<'a> {
    NewRule {
        name,
        format: Format::Npm,
        patterns,
        except: &[],
        effect,
    }
}

#[tokio::test]
async fn a_rule_round_trips_through_its_columns() {
    let fx = Fx::new().await;
    let store = store(&fx).await;
    let patterns = vec!["@acme/*".to_string(), "acme-*".to_string()];
    let effect = Effect::Members(vec!["inc-1".to_string(), "inc-2".to_string()]);
    let written = store
        .create(
            &NewRule {
                name: "internal",
                format: Format::Npm,
                patterns: &patterns,
                except: &["@acme/public-ui".to_string()],
                effect: &effect,
            },
            now(),
        )
        .await
        .unwrap();
    assert_eq!(written.patterns, patterns);
    assert_eq!(written.except, vec!["@acme/public-ui".to_string()]);
    assert_eq!(written.effect, effect);
    assert_eq!(written.format, Format::Npm);
    assert_eq!(store.by_name("internal").await.unwrap(), Some(written));
    assert!(store.by_name("absent").await.unwrap().is_none());
}

#[tokio::test]
async fn every_write_moves_the_snapshot_version_and_nothing_else_does() {
    let fx = Fx::new().await;
    let store = store(&fx).await;
    let patterns = vec!["@acme/*".to_string()];
    let start = store.version().await.unwrap();

    store.create(&rule("a", &patterns, &Effect::Deny), now()).await.unwrap();
    let after_create = store.version().await.unwrap();
    assert!(after_create > start);

    store.all().await.unwrap();
    assert_eq!(store.version().await.unwrap(), after_create, "a read moves nothing");

    store
        .update(&rule("a", &patterns, &Effect::AnyHosted), now())
        .await
        .unwrap();
    let after_update = store.version().await.unwrap();
    assert!(after_update > after_create);

    store.delete("a").await.unwrap();
    assert!(store.version().await.unwrap() > after_update);
}

#[tokio::test]
async fn a_name_is_taken_once_and_an_absent_one_is_never_written() {
    let fx = Fx::new().await;
    let store = store(&fx).await;
    let patterns = vec!["@acme/*".to_string()];
    store.create(&rule("a", &patterns, &Effect::Deny), now()).await.unwrap();
    assert!(matches!(
        store.create(&rule("a", &patterns, &Effect::Deny), now()).await,
        Err(StoreError::Conflict)
    ));
    assert!(matches!(
        store.update(&rule("b", &patterns, &Effect::Deny), now()).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(store.delete("b").await, Err(StoreError::NotFound)));
}

#[tokio::test]
async fn seeding_fills_an_empty_table_once_and_never_speaks_again() {
    let fx = Fx::new().await;
    let store = store(&fx).await;
    let patterns = vec!["@acme/*".to_string()];
    let seed = vec![rule("a", &patterns, &Effect::Deny), rule("b", &patterns, &Effect::Deny)];
    assert_eq!(store.ensure_seeded(&seed, now()).await.unwrap(), 2);
    let after = store.version().await.unwrap();

    assert_eq!(store.ensure_seeded(&seed, now()).await.unwrap(), 0);
    assert_eq!(store.version().await.unwrap(), after, "a seeded table is written once");

    // A rule an operator deleted stays deleted: by name, it would come back
    // at every restart, which is why the table's emptiness is the test.
    store.delete("a").await.unwrap();
    let hardened = vec![
        rule("a", &patterns, &Effect::AnyHosted),
        rule("b", &patterns, &Effect::AnyHosted),
    ];
    assert_eq!(store.ensure_seeded(&hardened, now()).await.unwrap(), 0);
    assert!(store.by_name("a").await.unwrap().is_none());
    assert_eq!(
        store.by_name("b").await.unwrap().unwrap().effect,
        Effect::Deny,
        "hardening a pattern in the file has no effect on a live deployment"
    );

    let names: Vec<String> = store.all().await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, vec!["b".to_string()], "in name order");
}
