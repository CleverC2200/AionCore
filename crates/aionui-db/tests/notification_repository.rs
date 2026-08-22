use aionui_db::{
    INotificationRepository, ReplaceNotificationSnapshotParams, SqliteNotificationRepository,
    StoreNotificationReceiptParams, UpsertNotificationParams, init_database_memory,
};

const USER_ID: &str = "system_default_user";

fn item(id: &str, version: &str, status: &str) -> UpsertNotificationParams {
    UpsertNotificationParams {
        notification_id: id.to_owned(),
        version: version.to_owned(),
        status: status.to_owned(),
        kind: "event".to_owned(),
        severity: "warning".to_owned(),
        title: format!("Notification {id}"),
        summary: Some("Summary".to_owned()),
        body: Some("Body".to_owned()),
        dismissible: true,
        source: "gea.workflow".to_owned(),
        target: r#"{"type":"notification"}"#.to_owned(),
        interaction_request_id: None,
        created_at: "2026-08-22T08:00:00Z".to_owned(),
        expires_at: None,
    }
}

#[tokio::test]
async fn snapshot_replacement_is_revision_aware_and_tenant_scoped() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteNotificationRepository::new(db.pool().clone());
    let tenant_a = ReplaceNotificationSnapshotParams {
        user_id: USER_ID.to_owned(),
        tenant_id: "tenant-a".to_owned(),
        revision: "r1".to_owned(),
        items: vec![item("notification-a", "v1", "unread")],
        synced_at: 1,
    };
    let tenant_b = ReplaceNotificationSnapshotParams {
        user_id: USER_ID.to_owned(),
        tenant_id: "tenant-b".to_owned(),
        revision: "r1".to_owned(),
        items: vec![item("notification-b", "v1", "unread")],
        synced_at: 1,
    };

    assert!(repo.replace_snapshot(&tenant_a).await.unwrap());
    assert!(!repo.replace_snapshot(&tenant_a).await.unwrap());
    assert!(repo.replace_snapshot(&tenant_b).await.unwrap());
    assert_eq!(repo.list(USER_ID, "tenant-a", Some("active")).await.unwrap().len(), 1);
    assert_eq!(repo.list(USER_ID, "tenant-b", Some("active")).await.unwrap().len(), 1);

    assert!(
        repo.replace_snapshot(&ReplaceNotificationSnapshotParams {
            revision: "r2".to_owned(),
            items: Vec::new(),
            synced_at: 2,
            ..tenant_a
        })
        .await
        .unwrap()
    );
    assert!(repo.list(USER_ID, "tenant-a", Some("all")).await.unwrap().is_empty());
    assert_eq!(repo.list(USER_ID, "tenant-b", Some("all")).await.unwrap().len(), 1);
}

#[tokio::test]
async fn receipt_and_projection_status_commit_atomically_and_replay_by_intent() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteNotificationRepository::new(db.pool().clone());
    repo.replace_snapshot(&ReplaceNotificationSnapshotParams {
        user_id: USER_ID.to_owned(),
        tenant_id: "tenant-a".to_owned(),
        revision: "r1".to_owned(),
        items: vec![item("notification-a", "v1", "unread")],
        synced_at: 1,
    })
    .await
    .unwrap();

    let receipt = r#"{"receipt_id":"receipt-1","notification_id":"notification-a","version":"v2","status":"read"}"#;
    repo.store_receipt_and_update(&StoreNotificationReceiptParams {
        user_id: USER_ID.to_owned(),
        tenant_id: "tenant-a".to_owned(),
        notification_id: "notification-a".to_owned(),
        idempotency_key: "command-1".to_owned(),
        expected_version: "v1".to_owned(),
        action: "read".to_owned(),
        receipt: receipt.to_owned(),
        created_at: 2,
        version: "v2".to_owned(),
        status: "read".to_owned(),
    })
    .await
    .unwrap();

    let row = repo.find(USER_ID, "tenant-a", "notification-a").await.unwrap().unwrap();
    assert_eq!(row.version, "v2");
    assert_eq!(row.status, "read");
    assert_eq!(
        repo.load_receipt(USER_ID, "tenant-a", "notification-a", "command-1")
            .await
            .unwrap()
            .unwrap()
            .receipt,
        receipt
    );
    assert!(
        repo.load_equivalent_receipt(USER_ID, "tenant-a", "notification-a", "v1", "read")
            .await
            .unwrap()
            .is_some()
    );
}
