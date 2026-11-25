const NAMED_EVENT_KINDS: [&str; 34] = [
    "mission.created",
    "mission.started",
    "plan.created",
    "plan.revised",
    "plan.degraded",
    "node.ready",
    "node.started",
    "node.turn",
    "node.thought",
    "node.tool_call",
    "node.tool_started",
    "node.tool_result",
    "node.approval_requested",
    "node.approval_decided",
    "node.needs_recovery",
    "node.succeeded",
    "node.failed",
    "node.budget_exceeded",
    "node.cancelled",
    "node.skipped",
    "mission.summary",
    "mission.done",
    "mission.failed",
    "mission.cancelled",
    "message.user",
    "routine.created",
    "routine.updated",
    "routine.deleted",
    "routine.triggered",
    "routine.error",
    "artifact.saved",
    "handoff.created",
    "model.retry",
    "model.fallback",
];

#[test]
fn console_registers_named_sse_event_listeners() {
    let html = include_str!("../web/index.html");

    for kind in NAMED_EVENT_KINDS {
        assert!(
            html.contains(&format!("'{kind}'")),
            "console is missing a listener for {kind}"
        );
    }

    assert!(html.contains("source.addEventListener(kind, handleEnvelope)"));
    assert!(html.contains("source.onmessage = handleEnvelope"));
    assert!(html.contains("health.retained_since_seq"));
}
