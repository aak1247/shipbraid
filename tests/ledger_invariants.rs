use auto_deliver::{
    domain::{Event, Lane, State},
    engine::Engine,
    store::Store,
};
use chrono::Utc;
use std::collections::BTreeSet;
use uuid::Uuid;

fn lane() -> Lane {
    Lane {
        id: "test".into(),
        project: "fixture".into(),
        environment: "test".into(),
        enabled: true,
        repository_id: "fixture".into(),
        release_ref: "refs/heads/main".into(),
        release_line_id: "fixture:main".into(),
        profile_digest: "original".into(),
        config_version: 1,
        descendant_coverage_enabled: false,
        preemption_enabled: false,
        allowed_revisions: BTreeSet::new(),
        allowed_components: BTreeSet::from(["app".into()]),
        active_run_id: None,
        generation: 0,
        blocked_reason: None,
        failure_paused: false,
    }
}

#[tokio::test]
async fn failed_commit_rolls_back_entity_updates_and_appended_audit_together() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("ledger.db")).await.unwrap();
    let empty = State::default();
    let mut baseline = empty.clone();
    Engine::default()
        .register_lane(&mut baseline, lane(), Utc::now())
        .unwrap();
    store.commit_transition(&empty, &baseline).await.unwrap();
    let mut invalid = baseline.clone();
    invalid.lanes.get_mut("test").unwrap().profile_digest = "must-rollback".into();
    let sequence = invalid.next_event_sequence["test"];
    invalid
        .next_event_sequence
        .insert("test".into(), sequence + 1);
    invalid.events.push(Event {
        sequence,
        lane_id: "test".into(),
        run_id: None,
        request_id: Some(Uuid::new_v4()), // Deferred FK deliberately fails at COMMIT.
        timestamp: Utc::now(),
        kind: "fault_injection".into(),
        message: "synthetic FK fault".into(),
    });
    assert!(store.commit_transition(&baseline, &invalid).await.is_err());
    assert_eq!(store.load_state().await.unwrap(), baseline);
    let pragmas = store.pragma_status().await.unwrap();
    assert!(pragmas.foreign_keys && pragmas.synchronous_full && pragmas.wal);
}

#[tokio::test]
async fn history_edits_are_rejected_without_changing_the_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("ledger.db")).await.unwrap();
    let empty = State::default();
    let mut baseline = empty.clone();
    Engine::default()
        .register_lane(&mut baseline, lane(), Utc::now())
        .unwrap();
    store.commit_transition(&empty, &baseline).await.unwrap();
    let mut rewritten = baseline.clone();
    rewritten.events[0].message = "rewritten history".into();
    assert!(
        store
            .commit_transition(&baseline, &rewritten)
            .await
            .is_err()
    );
    assert_eq!(store.load_state().await.unwrap(), baseline);
}
