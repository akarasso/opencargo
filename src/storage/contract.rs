//! `storage_contract!`: what every `StorageBackend` must answer the same way.
//! `$make` is an async block yielding `(guard, Arc<dyn StorageBackend>)`; the
//! guard keeps whatever backs the store alive for the test.

#[macro_export]
macro_rules! storage_contract {
    ($make:expr) => {
        use std::sync::Arc;

        use bytes::Bytes;
        use futures_util::StreamExt;
        use tokio::io::AsyncReadExt;

        use $crate::storage::{StorageBackend, StorageError};

        async fn keys(s: &Arc<dyn StorageBackend>, prefix: &str) -> Vec<String> {
            let mut keys: Vec<String> = s
                .list(prefix)
                .map(|m| m.unwrap().key)
                .collect()
                .await;
            keys.sort();
            keys
        }

        #[tokio::test]
        async fn put_then_get_round_trips() {
            let (_g, s) = $make.await;
            s.put("a/b/c", Bytes::from_static(b"body")).await.unwrap();
            assert_eq!(s.get("a/b/c").await.unwrap().as_ref(), b"body");
            let meta = s.stat("a/b/c").await.unwrap().unwrap();
            assert_eq!((meta.key.as_str(), meta.size), ("a/b/c", 4));
            assert!(matches!(s.get("a/b/none").await, Err(StorageError::NotFound)));
            assert!(s.stat("a/b/none").await.unwrap().is_none());
        }

        #[tokio::test]
        async fn uncommitted_bytes_are_never_listed() {
            let (_g, s) = $make.await;
            let mut w = s.writer("p/obj").await.unwrap();
            w.reserve(5).await.unwrap();
            w.write(Bytes::from_static(b"half-")).await.unwrap();
            assert!(keys(&s, "").await.is_empty(), "nothing listed mid-write");
            assert!(s.stat("p/obj").await.unwrap().is_none());
            w.write(Bytes::from_static(b"done")).await.unwrap();
            assert_eq!(w.commit().await.unwrap(), 9);
            assert_eq!(keys(&s, "").await, vec!["p/obj".to_string()]);
        }

        #[tokio::test]
        async fn a_dropped_writer_leaves_no_object() {
            let (_g, s) = $make.await;
            let mut w = s.writer("p/dropped").await.unwrap();
            w.write(Bytes::from_static(b"x")).await.unwrap();
            drop(w);
            assert!(s.stat("p/dropped").await.unwrap().is_none());
            assert!(keys(&s, "").await.is_empty());
        }

        #[tokio::test]
        async fn a_reader_survives_an_overwrite() {
            let (_g, s) = $make.await;
            s.put("o", Bytes::from_static(b"first")).await.unwrap();
            let mut read = s.read_stream("o").await.unwrap();
            assert_eq!(read.total, 5);
            s.put("o", Bytes::from_static(b"the second")).await.unwrap();
            let mut held = Vec::new();
            read.body.read_to_end(&mut held).await.unwrap();
            assert_eq!(held, b"first");
            assert_eq!(s.get("o").await.unwrap().as_ref(), b"the second");
        }

        #[tokio::test]
        async fn copy_keeps_the_source_and_relocate_need_not() {
            let (_g, s) = $make.await;
            s.put("src/x", Bytes::from_static(b"12345")).await.unwrap();
            s.copy_object("src/x", "dst/x").await.unwrap();
            assert_eq!(s.stat("src/x").await.unwrap().unwrap().size, 5);
            assert_eq!(s.stat("dst/x").await.unwrap().unwrap().size, 5);
            s.relocate("src/x", "moved/x").await.unwrap();
            assert_eq!(s.get("moved/x").await.unwrap().as_ref(), b"12345");
            s.relocate("src/x", "moved/x").await.unwrap();
            assert!(matches!(
                s.copy_object("src/none", "dst/none").await,
                Err(StorageError::NotFound)
            ));
        }

        #[tokio::test]
        async fn relocated_object_is_inside_the_grace_window() {
            let (_g, s) = $make.await;
            s.put("old/x", Bytes::from_static(b"x")).await.unwrap();
            let before = chrono::Utc::now() - chrono::TimeDelta::seconds(1);
            s.relocate("old/x", "new/x").await.unwrap();
            let meta = s.stat("new/x").await.unwrap().unwrap();
            assert!(meta.last_modified >= before, "{:?} < {before:?}", meta.last_modified);
        }

        #[tokio::test]
        async fn delete_batch_spares_a_sibling_prefix() {
            let (_g, s) = $make.await;
            for k in ["r/a/1", "r/a/2", "r/ab/1"] {
                s.put(k, Bytes::from_static(b"x")).await.unwrap();
            }
            let doomed = keys(&s, "r/a").await;
            assert_eq!(doomed, vec!["r/a/1".to_string(), "r/a/2".to_string()]);
            s.delete_batch(&doomed).await.unwrap();
            s.delete("r/a/1").await.unwrap();
            assert_eq!(keys(&s, "r").await, vec!["r/ab/1".to_string()]);
        }

        #[tokio::test]
        async fn list_is_segment_wise_and_includes_the_key_equal_to_the_prefix() {
            let (_g, s) = $make.await;
            for k in ["k", "kid", "q/leaf"] {
                s.put(k, Bytes::from_static(b"x")).await.unwrap();
            }
            assert_eq!(keys(&s, "k").await, vec!["k".to_string()]);
            assert_eq!(keys(&s, "q").await, vec!["q/leaf".to_string()]);
            assert_eq!(keys(&s, "q/leaf").await, vec!["q/leaf".to_string()]);
            assert!(keys(&s, "nothing").await.is_empty());
        }

        #[tokio::test]
        async fn keys_are_rejected_before_any_io() {
            let (_g, s) = $make.await;
            for bad in ["", "../x", "a//b", "_scratch/x", "_backend/health"] {
                assert!(
                    matches!(s.get(bad).await, Err(StorageError::InvalidPath(_))),
                    "{bad:?}"
                );
                assert!(matches!(
                    s.put(bad, Bytes::new()).await,
                    Err(StorageError::InvalidPath(_))
                ));
            }
            let listed: Vec<_> = s.list("../x").collect().await;
            assert!(matches!(listed.as_slice(), [Err(StorageError::InvalidPath(_))]));
        }

        #[tokio::test]
        async fn reserved_objects_are_never_listed() {
            let (_g, s) = $make.await;
            s.probe().await.unwrap();
            let report = s.self_check().await;
            assert!(report.ok(), "{report:?}");
            assert!(keys(&s, "").await.is_empty());
        }

        #[tokio::test]
        async fn sweep_leaves_committed_objects() {
            let (_g, s) = $make.await;
            s.put("kept", Bytes::from_static(b"x")).await.unwrap();
            let future = chrono::Utc::now() + chrono::TimeDelta::days(1);
            s.sweep_abandoned(std::time::Duration::from_secs(1), future)
                .await
                .unwrap();
            assert!(s.stat("kept").await.unwrap().is_some());
        }
    };
}
