use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::backend_manager::{new_child_pgids, retain_utf8_tail, Backend, BackendEvent};

#[test]
fn stderr_tail_truncation_preserves_utf8_boundaries() {
    let mut text = format!("{}—中文🔥tail", "a".repeat(111));

    retain_utf8_tail(&mut text, 12);

    assert_eq!(text, "文🔥tail");
    assert!(text.len() <= 12);
}

#[test]
fn stderr_tail_truncation_keeps_short_text_unchanged() {
    let mut text = "火星文".to_string();

    retain_utf8_tail(&mut text, 64);

    assert_eq!(text, "火星文");
}

#[tokio::test]
async fn late_response_after_cancel_is_discarded_and_tombstone_is_cleared() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let child_pgids = new_child_pgids();
    let request_marker = std::env::temp_dir().join(format!(
        "mcp-wrapper-backend-request-marker-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&request_marker);
    let backend = Arc::new(
        Backend::spawn(
            "python3",
            &[
                "tests/fixtures/echo_server.py".to_string(),
                "--respond-after-cancel".to_string(),
                "--request-marker".to_string(),
                request_marker.to_string_lossy().to_string(),
            ],
            tx,
            &child_pgids,
        )
        .expect("backend should spawn"),
    );

    let id = json!(1);
    let request = crate::mcp_interface::build_request(
        id.clone(),
        "tools/call",
        Some(json!({
            "name": "wait-cancel",
            "arguments": {}
        })),
    );
    let request_task = tokio::spawn({
        let backend = backend.clone();
        async move { backend.send_request(request).await }
    });

    let marker_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while tokio::time::Instant::now() < marker_deadline && !request_marker.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        request_marker.exists(),
        "backend should receive request before cancel"
    );
    assert!(backend.cancel_pending_request(&id, "test cancel").await);

    let response = tokio::time::timeout(std::time::Duration::from_secs(1), request_task)
        .await
        .expect("request task should finish")
        .expect("request task should join");
    assert!(response.is_err());

    let mut saw_unmatched = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
            Ok(Some(BackendEvent::UnmatchedResponse(_))) => saw_unmatched = true,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
        if backend.discarded_response_count().await == 0 {
            break;
        }
    }

    assert!(
        !saw_unmatched,
        "late cancelled response must not be unmatched"
    );
    assert_eq!(backend.discarded_response_count().await, 0);
    backend.kill().await;
    let _ = std::fs::remove_file(request_marker);
}

#[tokio::test]
async fn process_exit_drains_pending_request_without_request_timeout() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let child_pgids = new_child_pgids();
    let backend = Arc::new(
        Backend::spawn(
            "python3",
            &[
                "-c".to_string(),
                "import sys; sys.stdin.readline()".to_string(),
            ],
            tx,
            &child_pgids,
        )
        .expect("backend should spawn"),
    );
    let request = crate::mcp_interface::build_request(json!(1), "ping", None);

    let request_task = tokio::spawn({
        let backend = backend.clone();
        async move { backend.send_request(request).await }
    });

    let response = tokio::time::timeout(std::time::Duration::from_secs(1), request_task)
        .await
        .expect("request must terminate when backend stdout closes")
        .expect("request task should join");
    assert_eq!(
        response
            .expect_err("backend exit must fail the pending request")
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(backend.pending_response_count().await, 0);
    assert!(matches!(rx.recv().await, Some(BackendEvent::ProcessExit)));
}

#[tokio::test]
async fn backend_reader_does_not_wait_for_progress_consumer_before_response() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let child_pgids = new_child_pgids();
    let backend = Backend::spawn(
        "python3",
        &["-c".to_string(), "import sys,json; r=json.loads(sys.stdin.readline()); print(json.dumps({'jsonrpc':'2.0','method':'notifications/progress','params':{'progressToken':'p','progress':1}}), flush=True); print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{}}), flush=True)".to_string()],
        tx,
        &child_pgids,
    )
    .expect("backend should spawn");

    let backend = Arc::new(backend);
    let request = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .send_request(crate::mcp_interface::build_request(json!(1), "ping", None))
                .await
        }
    });
    let ack = match rx.recv().await {
        Some(BackendEvent::Notification(_, ack)) => ack,
        _ => panic!("progress notification should be emitted"),
    };
    drop(ack);
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), request)
        .await
        .expect("response completion must not block the backend reader")
        .expect("request task should join")
        .expect("backend response should succeed");
    assert_eq!(response["id"], json!(1));
    backend.kill().await;
}

#[tokio::test]
async fn discarded_response_suppression_is_not_evicted_by_volume() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let child_pgids = new_child_pgids();
    let backend = Backend::spawn(
        "python3",
        &["-c".to_string(), "import time; time.sleep(5)".to_string()],
        tx,
        &child_pgids,
    )
    .expect("backend should spawn");

    for id in 0..1_100 {
        backend.mark_discarding_for_test(json!(id)).await;
    }
    assert_eq!(backend.discarded_response_count().await, 1_100);
    assert!(backend.is_discarding_response(&json!(0)).await);
    assert!(backend.is_discarding_response(&json!(1099)).await);
    backend.kill().await;
}
