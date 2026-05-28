use crate::daemon_manager::{broker_status, daemon_paths, list_brokers};
use crate::logging::cmd_hash;

#[test]
fn daemon_paths_produces_hash_based_paths() {
    let paths = daemon_paths("python3", &["server.py".to_string()]);
    let hash = cmd_hash("python3", &["server.py".to_string()]);
    assert!(paths.socket.to_str().unwrap().contains(&hash));
    assert!(paths.socket.to_str().unwrap().ends_with(".sock"));
    assert!(paths.lock.to_str().unwrap().ends_with(".lock"));
    assert!(paths.pid.to_str().unwrap().ends_with(".pid"));
    assert!(paths.meta.to_str().unwrap().ends_with(".meta.json"));
}

#[test]
fn daemon_paths_different_args_different_hash() {
    let p1 = daemon_paths("python3", &["a.py".to_string()]);
    let p2 = daemon_paths("python3", &["b.py".to_string()]);
    assert_ne!(p1.socket, p2.socket);
}

#[test]
fn broker_status_is_read_only_for_missing_broker() {
    let paths = daemon_paths(
        "python3",
        &[format!("missing-broker-status-{}", std::process::id())],
    );
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.meta);

    let status = broker_status(&paths);
    assert!(!status.running);
    assert!(status.pid.is_none());
    assert!(!status.socket_exists);
    assert!(status.meta.is_none());
}

#[test]
fn list_brokers_handles_missing_or_empty_runtime_dir() {
    let brokers = list_brokers().expect("list brokers");
    assert!(brokers.iter().all(|b| !b.hash.is_empty()));
}
