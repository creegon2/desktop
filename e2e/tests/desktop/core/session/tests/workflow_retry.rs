//! Automatic retry of failed agent attempts through the public workflow commands, the real
//! session driver, and the fake ACP agent.

use super::workflow_resume::{run_case, wait_run_status};
use super::{agent_ref, install_fake_opencode_plugin, main_workspace_id, open_ready_backend};
use crate::setup::DesktopTestSetup;
use ora_contracts::*;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// `start → worker → output`, where `worker` has the given agent config.
fn single_agent_graph(agent_config: Value) -> String {
    json!({
        "nodes": [
            {"id": "start", "data": {"kind": "start"}},
            {"id": "worker", "data": {"kind": "agent", "agentConfig": agent_config}},
            {"id": "output", "data": {"kind": "output"}}
        ],
        "edges": [
            {"source": "start", "target": "worker"},
            {"source": "worker", "target": "output"}
        ]
    })
    .to_string()
}

/// Publishes `graph`, starts a run of it, and waits for it to succeed; returns its detail.
async fn run_to_success(
    setup: &DesktopTestSetup,
    graph: String,
) -> Result<GetWorkflowRunResponse, Box<dyn std::error::Error>> {
    let backend = open_ready_backend(setup)?;
    let workspace = setup.root().join("workspace");
    fs::create_dir_all(&workspace)?;
    backend.projects().create(CreateProjectRequest {
        name: "Retry E2E".to_string(),
        main_workspace_path: workspace.to_string_lossy().into_owned(),
    })?;
    let workspace_id = main_workspace_id(&backend)?;
    let workflow = backend
        .workflows()
        .create(CreateWorkflowRequest {
            name: "Retry".to_string(),
            graph: Some(graph),
        })?
        .workflow;
    backend.workflows().publish(PublishWorkflowRequest {
        workflow_id: workflow.id.clone(),
        version: Some("v1".to_string()),
    })?;
    let runs = backend.workflow_runs();
    let run = runs
        .create(CreateWorkflowRunRequest {
            workspace_id,
            workflow_id: workflow.id,
            locale: WorkflowRunLocale::EnUs,
            snapshot_id: None,
            kickoff_input: None,
            name: None,
            inject_last_failure: None,
        })?
        .run;
    runs.start(StartWorkflowRunRequest {
        run_id: run.id.clone(),
    })?;
    wait_run_status(&runs, &run.id, WorkflowRunStatus::Succeeded).await?;
    Ok(runs.get(GetWorkflowRunRequest { run_id: run.id })?)
}

/// The sessions the fake agent served a `session/prompt` for, in order.
fn prompted_sessions(package_root: &Path) -> Vec<String> {
    fs::read_to_string(package_root.join("acp_calls.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("session/prompt "))
        .map(str::to_string)
        .collect()
}

/// The payload of the single live `worker` row.
fn worker_payload(detail: &GetWorkflowRunResponse) -> Result<Value, Box<dyn std::error::Error>> {
    let workers: Vec<_> = detail
        .nodes
        .iter()
        .filter(|node| node.node_id == "worker")
        .collect();
    assert_eq!(workers.len(), 1, "{:?}", detail.nodes);
    assert_eq!(workers[0].status, WorkflowNodeStatus::Succeeded);
    Ok(serde_json::from_str(
        workers[0]
            .payload
            .as_deref()
            .ok_or("missing worker payload")?,
    )?)
}

/// E3: the agent's first prompt fails; the node retries by itself in a fresh session with no
/// failure block, the run succeeds without a resume, and the failed attempt stays readable.
#[test]
fn a_failed_session_is_retried_automatically_and_the_run_succeeds() -> TestResult {
    run_case(async {
        let setup = DesktopTestSetup::new()?;
        let package_root = install_fake_opencode_plugin(&setup.backend_paths().home_directory)?;
        let detail = run_to_success(
            &setup,
            single_agent_graph(json!({
                "executor": {"agentCli": agent_ref(), "modelId": "anthropic/claude-sonnet-4"},
                "prompt": "[fail-times:1] build it",
                "retry": {"enabled": true, "maxRetries": 2, "initialDelaySeconds": 0}
            })),
        )
        .await?;

        let failed = detail.failed_attempts.clone().unwrap_or_default();
        assert_eq!(
            failed
                .iter()
                .map(|attempt| (
                    attempt.node_id.as_str(),
                    attempt.attempt,
                    attempt.kind.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("worker", 1, "session")]
        );
        // Exactly the one planned prompt failure happened; the retry's prompt went through.
        assert_eq!(
            fs::read_to_string(package_root.join("prompt_failures.txt"))?,
            "[fail-times:1]\n"
        );
        let payload = worker_payload(&detail)?;
        assert_eq!(
            (
                &payload["auto_retry"],
                payload.get("injected_failure_context")
            ),
            (&json!({"retry": 1, "max_retries": 2}), None),
            "{payload}"
        );
        let sessions = prompted_sessions(&package_root);
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        assert_ne!(sessions[0], sessions[1]);
        // Ora's own session ids: the failed attempt keeps its session, the retry has another.
        let live_session = detail
            .nodes
            .iter()
            .find(|node| node.node_id == "worker")
            .and_then(|node| node.session_id.clone());
        assert!(failed[0].session_id.is_some() && live_session.is_some());
        assert_ne!(failed[0].session_id, live_session);
        Ok(())
    })
}

/// E4: a structured-output failure is retried with the failure block injected once, which the
/// fake agent answers with valid JSON.
#[test]
fn a_structured_output_failure_is_retried_with_the_failure_injected() -> TestResult {
    run_case(async {
        let setup = DesktopTestSetup::new()?;
        let package_root = install_fake_opencode_plugin(&setup.backend_paths().home_directory)?;
        let detail = run_to_success(
            &setup,
            single_agent_graph(json!({
                "executor": {"agentCli": agent_ref(), "modelId": "anthropic/claude-sonnet-4"},
                "prompt": "answer in JSON",
                "retry": {"enabled": true, "maxRetries": 1, "initialDelaySeconds": 0},
                "outputContract": {
                    "type": "structured",
                    "textExposure": "includeFinalText",
                    "schema": {
                        "type": "object",
                        "properties": {"ok": {"type": "boolean"}},
                        "required": ["ok"]
                    }
                }
            })),
        )
        .await?;

        let failed = detail.failed_attempts.clone().unwrap_or_default();
        assert_eq!(
            failed
                .iter()
                .map(|attempt| (attempt.attempt, attempt.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "structured_output")]
        );
        let payload = worker_payload(&detail)?;
        let injected = payload["injected_failure_context"]
            .as_str()
            .ok_or("the retry must record the injected failure block")?;
        assert_eq!(
            injected.matches("Previous attempt").count(),
            1,
            "{injected}"
        );
        assert_eq!(prompted_sessions(&package_root).len(), 2);
        Ok(())
    })
}
