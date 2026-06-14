use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::backend_manager::BackendSlot;
use crate::tools::schema::{
    error_envelope, ok_envelope, InvocationContext, ToolAction, ToolInvocation,
};

pub async fn invoke(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    match invocation.action {
        ToolAction::BackendStatus => status(invocation, context).await,
        ToolAction::BackendPing => ping(invocation, context).await,
        ToolAction::BackendRefresh => refresh(invocation, context).await,
        ToolAction::BackendRestart => restart(invocation, context).await,
        ToolAction::BackendStop => stop(invocation, context).await,
    }
}

async fn status(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    let backend = context.backend_slot.snapshot().await;
    let mcp = context.cache.snapshot();
    ok_envelope(
        invocation.action.name(),
        &invocation.target,
        json!({
            "backend": backend.to_value(),
            "mcp": mcp.to_value(),
        }),
    )
}

async fn ping(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    let backend = context.backend_slot.snapshot().await;
    ok_envelope(
        invocation.action.name(),
        &invocation.target,
        json!({
            "ok": backend.alive,
            "backend": backend.to_value(),
        }),
    )
}

async fn refresh(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    let be = match context.backend_slot.live().await {
        Some(be) => be,
        None => {
            return error_envelope(
                invocation.action.name(),
                &invocation.target,
                "backendUnavailable",
                "backend unavailable for refresh",
            );
        }
    };
    let report = context
        .cache
        .refresh_all(&be, context.backend_slot.generation())
        .await;
    ok_envelope(
        invocation.action.name(),
        &invocation.target,
        report.to_value(),
    )
}

async fn restart(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    let force = invocation
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if BackendSlot::restart_blocked_by_active_calls(
        context.active_calls.load(Ordering::Acquire),
        force,
    ) {
        return error_envelope(
            invocation.action.name(),
            &invocation.target,
            "activeCalls",
            "backend restart rejected while calls are active",
        );
    }

    if let Err(e) = context.backend_slot.restart(force).await {
        return error_envelope(
            invocation.action.name(),
            &invocation.target,
            "backendRestart",
            &e,
        );
    }
    let be = match context.backend_slot.live().await {
        Some(be) => be,
        None => {
            return error_envelope(
                invocation.action.name(),
                &invocation.target,
                "backendUnavailable",
                "backend unavailable after restart",
            );
        }
    };
    let report = context
        .cache
        .refresh_all(&be, context.backend_slot.generation())
        .await;
    ok_envelope(
        invocation.action.name(),
        &invocation.target,
        report.to_value(),
    )
}

async fn stop(invocation: ToolInvocation, context: InvocationContext<'_>) -> Value {
    if BackendSlot::stop_blocked_by_active_calls(context.active_calls.load(Ordering::Acquire)) {
        return error_envelope(
            invocation.action.name(),
            &invocation.target,
            "activeCalls",
            "backend stop rejected while calls are active",
        );
    }
    context.backend_slot.stop().await;
    let backend = context.backend_slot.snapshot().await;
    ok_envelope(
        invocation.action.name(),
        &invocation.target,
        backend.to_value(),
    )
}
