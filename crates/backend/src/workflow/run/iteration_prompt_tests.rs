//! Iteration members must be dispatched with the payload committed by the round start.
//!
//! `iteration_tests` complete region rows by hand, so they never render `{{#iter.item#}}` through
//! the real `drive_agent_node` renderer. This suite uses that renderer on dispatch.

use super::test_fixture::{
    ClockAt, RecordingPromptExecutor, SeqGen, bootstrap, seeded_pending_run,
};
use ora_application::{WorkflowRunEngine, WorkflowRunEngineRepository, WorkflowRunPayload};
use ora_db::SqliteWorkflowRunEngineRepository;
use ora_domain::{WorkflowNodeStatus, WorkflowRunId, WorkflowRunStatus};
use pretty_assertions::assert_eq;
use serde_json::json;

const FOREACH_PROMPT_GRAPH: &str = r#"{"nodes":[
    {"id":"start","data":{"kind":"start","inputVariables":[
        {"name":"modules","valueType":"array[string]"}
    ]}},
    {"id":"iter","data":{"kind":"iteration","iterationConfig":{
        "iteratorSelector":["start","modules"],
        "collectSelector":["fix","output"],
        "errorStrategy":"fail"
    }}},
    {"id":"fix","parentId":"iter","data":{"kind":"agent","agentConfig":{
        "executor":{"agentCli":"open_code","modelId":"m"},
        "prompt":"item={{#iter.item#}} index={{#iter.index#}}"
    }}},
    {"id":"output","data":{"kind":"output","outputs":[
        {"name":"collected","variableSelector":["iter","output"]}
    ]}}
],"edges":[
    {"source":"start","target":"iter"},
    {"source":"iter","target":"fix"},
    {"source":"iter","target":"output"}
]}"#;

/// Completes every currently running region agent so the composite can start the next round.
fn complete_running_fix(
    engine: &WorkflowRunEngine<SqliteWorkflowRunEngineRepository, SeqGen, ClockAt>,
    repository: &SqliteWorkflowRunEngineRepository,
    run_id: &WorkflowRunId,
) {
    let rows: Vec<_> = repository
        .list_node_runs(run_id)
        .unwrap()
        .into_iter()
        .filter(|row| row.node_id == "fix" && row.status == WorkflowNodeStatus::Running)
        .collect();
    for row in rows {
        let output = format!("out-{}", row.iteration.unwrap_or_default());
        engine
            .complete_node(
                run_id,
                &row.id,
                Some(output),
                /*structured_output*/ None,
                /*stop_reason*/ None,
                Vec::new(),
            )
            .unwrap();
    }
}

/// Round bindings committed by `start_iteration_round` / `settle_iteration_round` must be in the
/// dispatched context; otherwise `drive_agent_node` fails with `iter.item` unset.
#[test]
fn foreach_member_prompt_renders_item_and_index_from_the_dispatched_context() {
    super::test_fixture::run_test(async {
        let (temp, pool) = bootstrap();
        let run_id = seeded_pending_run(&temp, &pool, FOREACH_PROMPT_GRAPH);
        let repository = SqliteWorkflowRunEngineRepository::new(pool.clone());
        let mut variables = std::collections::BTreeMap::new();
        variables.insert("modules".to_string(), json!(["a", "b"]));
        repository
            .update_run_input(&run_id, Some("kickoff".to_string()), variables, 35)
            .unwrap();

        let executor = RecordingPromptExecutor::default();
        let engine = WorkflowRunEngine::new(
            repository.clone(),
            executor.clone(),
            SeqGen::default(),
            ClockAt(40),
        );
        engine.start(&run_id).unwrap();
        complete_running_fix(&engine, &repository, &run_id);
        complete_running_fix(&engine, &repository, &run_id);

        let prompts = executor.prompts.lock().expect("prompt log").clone();
        assert_eq!(
            prompts,
            vec!["item=a index=0".to_string(), "item=b index=1".to_string(),]
        );

        let context = repository
            .find_execution_context(&run_id)
            .unwrap()
            .expect("run context");
        assert_eq!(context.run.status, WorkflowRunStatus::Succeeded);
        let payload: WorkflowRunPayload =
            serde_json::from_str(context.run.payload.as_deref().expect("run payload")).unwrap();
        assert_eq!(
            payload.variable_pool.values.get("iter.output"),
            Some(&json!(["out-0", "out-1"]))
        );
    });
}
