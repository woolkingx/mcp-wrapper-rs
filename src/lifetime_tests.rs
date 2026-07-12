use serde_json::json;
use tokio::sync::mpsc;

use crate::backend_manager::{new_child_pgids, Backend};
use crate::lifetime::{translate_cancel_message, RequestLifetime};

#[tokio::test]
async fn peer_cancel_requires_exact_session_owner() {
    let lifetime = RequestLifetime::new();
    let peer_request = json!({
        "jsonrpc": "2.0",
        "id": "peer-1",
        "method": "sampling/createMessage",
        "params": {
            "_meta": {"progressToken": "peer-token"}
        }
    });
    lifetime.insert_peer_request(Some(7), &peer_request).await;

    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": "peer-1"}
    });

    assert!(!lifetime.client_cancel_peer_request(&cancel).await);
    assert!(
        !lifetime
            .client_cancel_peer_request_for_session(8, &cancel)
            .await
    );
    assert!(
        lifetime
            .client_cancel_peer_request_for_session(7, &cancel)
            .await
    );
}

#[tokio::test]
async fn peer_response_requires_exact_session_owner() {
    let lifetime = RequestLifetime::new();
    let peer_request = json!({
        "jsonrpc": "2.0",
        "id": "peer-2",
        "method": "sampling/createMessage",
        "params": {
            "_meta": {"progressToken": "peer-token-2"}
        }
    });
    lifetime.insert_peer_request(Some(9), &peer_request).await;

    let response = json!({
        "jsonrpc": "2.0",
        "id": "peer-2",
        "result": {"content": {"type": "text", "text": "ok"}}
    });

    assert!(!lifetime.take_peer_response(&response).await);
    assert!(!lifetime.take_peer_response_for_session(8, &response).await);
    assert!(lifetime.take_peer_response_for_session(9, &response).await);
    assert!(!lifetime.take_peer_response_for_session(9, &response).await);
}

#[tokio::test]
async fn equal_peer_ids_and_tokens_are_independent_per_session() {
    let lifetime = RequestLifetime::new();
    for session_id in [11, 22] {
        lifetime
            .insert_peer_request(
                Some(session_id),
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "sampling/createMessage",
                    "params": {"_meta": {"progressToken": "same-token"}}
                }),
            )
            .await;
    }

    let progress = json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": "same-token", "progress": 1}
    });
    assert!(
        lifetime
            .client_progress_is_active_for_session(11, &progress)
            .await
    );
    assert!(
        lifetime
            .client_progress_is_active_for_session(22, &progress)
            .await
    );
    assert_eq!(lifetime.peer_index_counts().await, (2, 2));

    let response = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
    assert!(lifetime.take_peer_response_for_session(22, &response).await);
    assert!(
        lifetime
            .client_progress_is_active_for_session(11, &progress)
            .await
    );
    assert!(
        !lifetime
            .client_progress_is_active_for_session(22, &progress)
            .await
    );
    assert_eq!(lifetime.peer_index_counts().await, (1, 1));
}

#[tokio::test]
async fn equal_raw_client_ids_are_owned_and_removed_per_session() {
    let lifetime = RequestLifetime::new();
    let raw_id = json!(1);
    lifetime
        .insert_with_session(
            Some(10),
            raw_id.clone(),
            json!(100),
            &json!({"params": {"_meta": {"progressToken": "token-a"}}}),
        )
        .await;
    lifetime
        .insert_with_session(
            Some(20),
            raw_id.clone(),
            json!(200),
            &json!({"params": {"_meta": {"progressToken": "token-b"}}}),
        )
        .await;

    lifetime.remove_client_for_session(20, &raw_id).await;

    assert!(lifetime.contains_client_for_session(10, &raw_id).await);
    assert!(!lifetime.contains_client_for_session(20, &raw_id).await);
    assert_eq!(lifetime.client_index_counts().await, (1, 1, 1));

    lifetime.timeout_client_for_session(10, &raw_id).await;
    assert_eq!(lifetime.client_index_counts().await, (0, 0, 0));
}

#[tokio::test]
async fn numeric_and_string_client_ids_remain_distinct_within_a_session() {
    let lifetime = RequestLifetime::new();
    lifetime
        .insert_with_session(Some(7), json!(1), json!(11), &json!({"params": {}}))
        .await;
    lifetime
        .insert_with_session(Some(7), json!("1"), json!(12), &json!({"params": {}}))
        .await;

    lifetime.remove_client_for_session(7, &json!(1)).await;

    assert!(!lifetime.contains_client_for_session(7, &json!(1)).await);
    assert!(lifetime.contains_client_for_session(7, &json!("1")).await);
    assert_eq!(lifetime.client_index_counts().await, (1, 1, 0));
}

#[tokio::test]
async fn active_request_queries_are_derived_from_rows() {
    let lifetime = RequestLifetime::new();
    lifetime
        .insert_with_session(Some(1), json!(1), json!(101), &json!({"params": {}}))
        .await;
    lifetime
        .insert_with_session(Some(1), json!(2), json!(102), &json!({"params": {}}))
        .await;

    assert_eq!(lifetime.active_count().await, 2);
    assert_eq!(lifetime.active_count_for_session(1).await, 2);
    assert_eq!(lifetime.active_count_for_session(2).await, 0);
    assert_eq!(lifetime.unique_bound_session().await, Some(1));

    assert!(
        lifetime
            .admit_with_session(Some(2), json!(1), &json!({"params": {}}))
            .await
    );
    assert_eq!(lifetime.active_count().await, 3);
    assert_eq!(lifetime.unique_bound_session().await, Some(1));

    lifetime.bind_backend(Some(2), &json!(1), json!(201)).await;
    assert_eq!(lifetime.active_count().await, 3);
    assert_eq!(lifetime.unique_bound_session().await, None);

    lifetime.remove_client_for_session(2, &json!(1)).await;
    assert_eq!(lifetime.unique_bound_session().await, Some(1));
}

#[tokio::test]
async fn daemon_cancel_uses_session_and_raw_id_without_fallback() {
    let lifetime = RequestLifetime::new();
    let (tx, _rx) = mpsc::unbounded_channel();
    let child_pgids = new_child_pgids();
    let backend = Backend::spawn(
        "python3",
        &["-c".to_string(), "import time; time.sleep(5)".to_string()],
        tx,
        &child_pgids,
    )
    .expect("backend should spawn");
    let raw_id = json!(1);
    lifetime
        .insert_with_session(Some(30), raw_id.clone(), json!(300), &json!({"params": {}}))
        .await;
    lifetime
        .insert_with_session(Some(40), raw_id.clone(), json!(400), &json!({"params": {}}))
        .await;
    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 1, "reason": "session-local cancel"}
    });

    assert!(
        !lifetime
            .cancel_client_request_for_session(50, &cancel, &backend)
            .await
    );
    assert!(lifetime.contains_client_for_session(30, &raw_id).await);
    assert!(lifetime.contains_client_for_session(40, &raw_id).await);

    // No backend pending sender was registered in this owner-local fixture, so
    // forwarding returns false after the exact session-owned lifetime row is removed.
    assert!(
        !lifetime
            .cancel_client_request_for_session(40, &cancel, &backend)
            .await
    );
    assert!(lifetime.contains_client_for_session(30, &raw_id).await);
    assert!(!lifetime.contains_client_for_session(40, &raw_id).await);
    assert_eq!(lifetime.client_index_counts().await, (1, 1, 0));

    assert!(
        lifetime
            .admit_with_session(Some(50), json!(2), &json!({"params": {}}))
            .await
    );
    let queued_cancel = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "cancel before backend bind"}
    });
    assert!(
        lifetime
            .cancel_client_request_for_session(50, &queued_cancel, &backend)
            .await
    );
    assert!(!lifetime.contains_client_for_session(50, &json!(2)).await);
    backend.kill().await;
}

#[test]
fn cancel_translation_requires_object_params() {
    let malformed = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": []
    });
    assert!(translate_cancel_message(&malformed, json!(1)).is_none());

    let valid = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": "backend-id", "reason": "stop"}
    });
    let translated = translate_cancel_message(&valid, json!("client-id")).unwrap();
    assert_eq!(translated["params"]["requestId"], json!("client-id"));
    assert_eq!(translated["params"]["reason"], json!("stop"));
}
