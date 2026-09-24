//! Regression tests for automatic-retry edge cases: which earlier failure a new
//! attempt is told about.

use super::recovery::sweep_one_run;
use super::retry_tests::{Harness, agent, graph, linear, retry};
use ora_application::{
    NodeFailure, NodeFailureKind, ResumeWorkflowRunResult, WorkflowRunEngineRepository,
};
use ora_domain::WorkflowRunStatus;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// A Loop (at most three rounds, until `writer` answers `done`) whose body is
/// `entry → writer → checker`; `checker` never retries, so its failure fails the Loop.
fn loop_with_checker() -> String {
    let body_agent = |id: &str, extra: Value| {
        let mut node = agent(id, extra);
        node["parentId"] = json!("loop");
        node["data"]["containerId"] = json!("loop");
        node
    };
    let nodes = vec![
        json!({"id": "start", "data": {"kind": "start"}}),
        json!({"id": "loop", "data": {"kind": "loop", "loopConfig": {
            "maxIterations": 3,
            "variables": [{"name": "draft", "valueType": "string",
                "initial": {"kind": "constant", "value": "seed"}, "feedback": ["writer", "output"]}],
            "until": {"logic": "and", "conditions": [
                {"variableSelector": ["writer", "output"], "operator": "equals", "value": "done"}
            ]},
            "outputs": [{"name": "result", "variableSelector": ["writer", "output"]}]
        }}}),
        json!({"id": "entry", "parentId": "loop", "data": {"kind": "start", "containerId": "loop"}}),
        body_agent("writer", json!({})),
        body_agent("checker", retry(false, 0, 0)),
    ];
    let mut document: Value = serde_json::from_str(&graph(
        nodes,
        &[
            ("start", "loop"),
            ("entry", "writer"),
            ("writer", "checker"),
        ],
    ))
    .unwrap();
    document["schemaVersion"] = json!(2);
    document.to_string()
}

fn structured_output_failure(message: &str) -> NodeFailure {
    NodeFailure::new(NodeFailureKind::StructuredOutput, message)
        .with_output(Some("plain text".to_string()))
}

/// A failure an automatic retry already fixed stays fixed when a resume clears the successful
/// retry. The Loop reruns from round 1, and its fresh `writer` must not be told about round 1's
/// first attempt, while `checker`, whose failure was never fixed, is.
#[test]
fn a_failure_a_retry_fixed_is_not_injected_again_after_a_resume_clears_the_fix() {
    let h = Harness::start(&loop_with_checker());
    let first = h.running("writer");
    h.fail_with(
        &first.id,
        structured_output_failure("reply is not JSON"),
        1_000,
    );
    let fixed = h.running("writer");
    h.wake(&fixed.id, 11_000);
    assert!(
        h.prompt_for("writer")
            .contains("Previous attempt (1) failed")
    );
    h.complete(&fixed.id, "draft", 12_000);
    let checker = h.running("checker");
    h.fail_with(
        &checker.id,
        structured_output_failure("checker reply is not JSON"),
        13_000,
    );
    assert_eq!(h.run().status, WorkflowRunStatus::Failed);

    h.set_now(14_000);
    assert_eq!(
        h.engine.resume_from_failure(&h.run_id).unwrap(),
        ResumeWorkflowRunResult::Resumed
    );
    let rerun = h.running("writer");
    assert_ne!(rerun.id, fixed.id);
    assert_eq!(
        h.repository()
            .find_last_failed_attempt(&h.run_id, "writer", rerun.iteration)
            .unwrap()
            .map(|row| row.id),
        None
    );
    let prompt = h.prompt_for("writer");
    assert!(!prompt.contains("Previous attempt"), "{prompt}");

    h.complete(&rerun.id, "draft", 15_000);
    let prompt = h.prompt_for("checker");
    assert!(prompt.contains("Previous attempt (1) failed"), "{prompt}");
    assert!(prompt.contains("checker reply is not JSON"), "{prompt}");
}

/// A restart during the wait fails the waiting row, which never ran. After a resume the new
/// attempt is still told about the attempt that really failed.
#[test]
fn a_restart_during_the_wait_keeps_the_real_failure_for_the_resumed_attempt() {
    let h = Harness::start(&linear(json!({})));
    h.fail_with(
        &h.running("a").id,
        structured_output_failure("reply is not JSON"),
        1_000,
    );
    let waiting = h.running("a");
    sweep_one_run(&h.repository(), &h.run_id, 5_000).unwrap();
    assert_eq!(
        (h.rows_of("a")[0].id.clone(), h.run().status),
        (waiting.id.clone(), WorkflowRunStatus::Failed)
    );

    h.set_now(20_000);
    assert_eq!(
        h.engine.resume_from_failure(&h.run_id).unwrap(),
        ResumeWorkflowRunResult::Resumed
    );
    let prompt = h.prompt_for("a");
    assert_eq!(prompt.matches("Previous attempt").count(), 1, "{prompt}");
    assert!(prompt.contains("Previous attempt (1) failed"), "{prompt}");
    assert!(prompt.contains("reply is not JSON"), "{prompt}");
}
