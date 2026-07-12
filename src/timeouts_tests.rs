use std::time::Duration;

use crate::timeouts::{PassThroughRequestTimeout, TimeoutSource};

#[test]
fn pass_through_timeout_defaults_to_current_behavior() {
    let timeout = PassThroughRequestTimeout::from_mcp_timeout_value(None).unwrap();
    assert_eq!(timeout.duration(), None);
    assert_eq!(timeout.source(), TimeoutSource::CurrentDefault);
}

#[test]
fn pass_through_timeout_reads_mcp_timeout_millis() {
    let timeout = PassThroughRequestTimeout::from_mcp_timeout_value(Some("250")).unwrap();
    assert_eq!(timeout.duration(), Some(Duration::from_millis(250)));
    assert_eq!(timeout.source(), TimeoutSource::UpstreamEnv);
}

#[test]
fn pass_through_timeout_rejects_zero_or_invalid_values() {
    assert!(PassThroughRequestTimeout::from_mcp_timeout_value(Some("0")).is_err());
    assert!(PassThroughRequestTimeout::from_mcp_timeout_value(Some("abc")).is_err());
}
