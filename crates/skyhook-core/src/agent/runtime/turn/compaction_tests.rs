//! End-to-end automatic compaction uses completed provider usage, not request estimates.

use super::*;
use crate::agent::runtime::tests::*;
use crate::session::ModelPurpose;

fn with_usage(mut chunks: Vec<ResponseChunk>, snapshots: &[Usage]) -> Vec<ResponseChunk> {
    let end = chunks.pop().expect("response has a terminal chunk");
    assert!(matches!(end, ResponseChunk::ResponseEnded { .. }));
    chunks.extend(
        snapshots
            .iter()
            .map(|usage| ResponseChunk::UsageUpdated { usage: *usage }),
    );
    chunks.push(end);
    chunks
}

fn threshold_usage() -> Usage {
    // Exactly 80% of the harness's 128,000-token context. Each component is
    // necessary: neither input alone nor input plus cache reaches the threshold.
    Usage {
        input_tokens: 80_000,
        cached_input_tokens: 20_000,
        output_tokens: 2_400,
    }
}

fn summary() -> Vec<ResponseChunk> {
    answer(
        json!({
            "objective": "Continue the investigation", "user_instructions": [],
            "session_rules": [], "plan": [], "findings": [], "open_issues": [],
            "running_work": [], "completed_work": [], "decisions": [],
            "recovery_details": [], "jobs": [], "additional_context": [],
            "todo_reconciliation": [], "todos": [],
            "resumption_point": "Use the retained results", "next_actions": []
        })
        .to_string(),
    )
}

async fn seed_history(session: &SessionHandle, repetitions: usize) {
    session
        .runtime
        .commit(
            &session.root,
            Message::Assistant(vec![AssistantContent::text(
                "old-history",
                0,
                "research ".repeat(repetitions),
            )]),
        )
        .await
        .unwrap();
}

fn purposes(records: &[EventRecord]) -> Vec<ModelPurpose> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ModelRequested { purpose, .. } => Some(*purpose),
            _ => None,
        })
        .collect()
}

fn shell_response() -> Vec<ResponseChunk> {
    response(vec![AssistantContent::tool_call(
        "tool",
        0,
        ToolCall {
            id: "append-once".into(),
            name: "shell".into(),
            arguments: json!({"command": "printf 'executed\\n' >> executions; printf retained-result"}),
        },
    )])
}

#[tokio::test]
async fn high_usage_tool_response_closes_exchange_before_summary_and_next_normal_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let harness = test_harness(
        workspace.path(),
        sessions.path(),
        scripted_provider(
            &requests,
            [
                with_usage(shell_response(), &[threshold_usage()]),
                summary(),
                answer("original final"),
            ],
        ),
    )
    .await;
    let session = harness.new_session().await.unwrap();
    // Enough old material to remove, but nowhere near the automatic threshold.
    seed_history(&session, 6_000).await;
    assert_eq!(
        session.prompt("Run the tool.").await.unwrap(),
        "original final"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("executions"))
            .await
            .unwrap(),
        "executed\n",
        "compaction must not re-execute the tool"
    );
    let records = session.runtime.store.records().await;
    assert_eq!(
        purposes(&records),
        vec![
            ModelPurpose::Agent,
            ModelPurpose::Compaction,
            ModelPurpose::Agent
        ]
    );
    let (tool_sequence, tool_message) = records
        .iter()
        .find_map(|record| match &record.event {
            SessionEvent::MessageCommitted {
                message: Message::Tool(results),
            } if results.iter().any(|result| result.call_id == "append-once") => {
                assert_eq!(results.len(), 1);
                assert!(!results[0].is_error);
                assert!(results[0].result.to_string().contains("retained-result"));
                Some((record.sequence, Message::Tool(results.clone())))
            }
            _ => None,
        })
        .expect("tool result committed");
    let summary_sequence = records
        .iter()
        .find(|record| {
            matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Compaction,
                    ..
                }
            )
        })
        .unwrap()
        .sequence;
    let checkpoint_sequence = records
        .iter()
        .find(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        .expect("closed exchange permits a checkpoint")
        .sequence;
    let next_sequence = records
        .iter()
        .rev()
        .find(|record| {
            matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Agent,
                    ..
                }
            )
        })
        .unwrap()
        .sequence;
    assert!(tool_sequence < summary_sequence);
    assert!(summary_sequence < checkpoint_sequence && checkpoint_sequence < next_sequence);
    {
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert!(captured[0].response_schema.is_none());
        assert!(captured[1].response_schema.is_some());
        assert!(captured[1].tools.is_empty());
        assert!(captured[2].response_schema.is_none());
        assert_eq!(captured[0].tools, captured[2].tools);
        for request in [&captured[1], &captured[2]] {
            let index = request
                .messages
                .iter()
                .position(|message| message == &tool_message)
                .expect("summary and continuation retain the actual tool result");
            assert!(
                matches!(&request.messages[index - 1], Message::Assistant(items)
                if items.iter().flat_map(|item| &item.blocks).any(|block|
                    matches!(&block.content, BlockContent::ToolCall(call) if call.id == "append-once")))
            );
        }
    }
    shutdown_session(session).await;
}

#[tokio::test]
async fn high_usage_final_response_compacts_before_returning_original_text_regardless_of_max_output()
 {
    for max_output in [1, 120_000] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_builder(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    with_usage(answer("original final"), &[threshold_usage()]),
                    summary(),
                ],
            ),
        )
        .model_profile(
            "test",
            ModelProfile {
                provider: "test".into(),
                model: "test".into(),
                reasoning: None,
                max_context: 128_000,
                max_output,
                supports_images: false,
            },
        )
        .build()
        .await
        .unwrap();
        let session = harness.new_session().await.unwrap();
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Finish.").await.unwrap(), "original final");
        // Capture live state immediately: no further request or journal catch-up
        // should be necessary to display the reduced context after compaction.
        let observation = session.runtime.events.observe().snapshot;
        let records = session.runtime.store.records().await;
        assert_eq!(
            purposes(&records),
            vec![ModelPurpose::Agent, ModelPurpose::Compaction]
        );
        let checkpoint = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::Compaction { checkpoint } => Some(checkpoint),
                _ => None,
            })
            .expect("checkpoint must already exist when prompt returns");
        let context = observation.context.get(&session.root).unwrap();
        assert!(checkpoint.after_tokens < checkpoint.before_tokens);
        assert_eq!(
            context.tokens, checkpoint.after_tokens,
            "final-response compaction must refresh live occupancy before returning"
        );
        assert_eq!(context.capacity, 128_000);
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 2);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].messages.iter().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("answer", 0, "original final")])));
        }
        shutdown_session(session).await;
    }
}

#[tokio::test]
async fn oversized_history_with_low_completed_usage_does_not_compact() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let harness = test_harness(
        workspace.path(),
        sessions.path(),
        scripted_provider(
            &requests,
            [with_usage(
                answer("done"),
                &[Usage {
                    input_tokens: 10,
                    cached_input_tokens: 5,
                    output_tokens: 1,
                }],
            )],
        ),
    )
    .await;
    let session = harness.new_session().await.unwrap();
    seed_history(&session, 100_000).await;
    assert_eq!(session.prompt("Finish.").await.unwrap(), "done");
    let records = session.runtime.store.records().await;
    assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    assert!(super::super::compaction::estimate_request(&requests.lock().unwrap()[0]) > 128_000);
    shutdown_session(session).await;
}

#[tokio::test]
async fn oversized_tool_result_with_low_completed_usage_does_not_compact() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let harness = test_harness(
        workspace.path(),
        sessions.path(),
        scripted_provider(
            &requests,
            [with_usage(
                answer("done"),
                &[Usage {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    output_tokens: 1,
                }],
            )],
        ),
    )
    .await;
    let session = harness.new_session().await.unwrap();
    seed_history(&session, 6_000).await;
    // Seed a complete historical exchange: output size must not substitute for
    // the provider's actual usage, even when this result alone exceeds context.
    session
        .runtime
        .commit(
            &session.root,
            Message::Assistant(vec![AssistantContent::tool_call(
                "large-tool",
                0,
                ToolCall {
                    id: "large-result".into(),
                    name: "read".into(),
                    arguments: json!({"path": "research.txt"}),
                },
            )]),
        )
        .await
        .unwrap();
    session
        .runtime
        .commit(
            &session.root,
            Message::Tool(vec![ToolResult {
                call_id: "large-result".into(),
                name: "read".into(),
                result: json!({"content": "research ".repeat(100_000)}),
                images: vec![],
                is_error: false,
            }]),
        )
        .await
        .unwrap();
    assert_eq!(session.prompt("Use the result.").await.unwrap(), "done");
    let records = session.runtime.store.records().await;
    assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    assert!(super::super::compaction::estimate_request(&requests.lock().unwrap()[0]) > 128_000);
    shutdown_session(session).await;
}

#[tokio::test]
async fn cumulative_usage_snapshots_and_separate_responses_are_not_added_for_compaction() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let snapshots = [
        Usage {
            input_tokens: 50_000,
            cached_input_tokens: 0,
            output_tokens: 0,
        },
        Usage {
            input_tokens: 50_000,
            cached_input_tokens: 5_000,
            output_tokens: 5_000,
        },
    ];
    let harness = test_builder(
        workspace.path(),
        sessions.path(),
        scripted_provider(
            &requests,
            [
                with_usage(shell_response(), &snapshots),
                with_usage(answer("done"), &snapshots),
            ],
        ),
    )
    .model_profile(
        "test",
        ModelProfile {
            provider: "test".into(),
            model: "test".into(),
            reasoning: None,
            max_context: 128_000,
            max_output: 120_000,
            supports_images: false,
        },
    )
    .build()
    .await
    .unwrap();
    let session = harness.new_session().await.unwrap();
    seed_history(&session, 6_000).await;
    assert_eq!(session.prompt("Run then finish.").await.unwrap(), "done");
    let records = session.runtime.store.records().await;
    assert_eq!(
        purposes(&records),
        vec![ModelPurpose::Agent, ModelPurpose::Agent]
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    let usage: Vec<_> = records
        .iter()
        .filter_map(|record| match record.event {
            SessionEvent::Usage { usage, .. } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage, vec![snapshots[1], snapshots[1]]);
    shutdown_session(session).await;
}
