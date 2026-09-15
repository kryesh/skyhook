//! End-to-end automatic compaction uses completed provider usage, not request estimates.

use super::*;
use crate::agent::runtime::tests::*;
use crate::session::ModelPurpose;

fn with_usage(mut chunks: Vec<ResponseChunk>, snapshots: &[Usage]) -> Vec<ResponseChunk> {
    let end = chunks.pop().expect("response has a terminal chunk");
    assert!(matches!(end, ResponseChunk::ResponseEnded { .. }));
    for &usage in snapshots {
        chunks.push(ResponseChunk::UsageUpdated { usage });
    }
    chunks.push(end);
    chunks
}

fn usage(input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
    }
}

// Exactly 80% of the harness's 128,000-token context. Each component is
// necessary: neither input alone nor input plus cache reaches the threshold.
const THRESHOLD: (u64, u64, u64) = (80_000, 20_000, 2_400);

fn threshold_usage() -> Usage {
    usage(THRESHOLD.0, THRESHOLD.1, THRESHOLD.2)
}

async fn seed_history(session: &SessionHandle, repetitions: usize) {
    let research = AssistantContent::text("old-history", 0, "research ".repeat(repetitions));
    let research = Message::Assistant(vec![research]);
    session
        .runtime
        .commit(&session.root, research)
        .await
        .unwrap();
}

fn purposes(records: &[EventRecord]) -> Vec<ModelPurpose> {
    events!(records, SessionEvent::ModelRequested { purpose, .. } => *purpose)
}

fn shell_response() -> Vec<ResponseChunk> {
    let command = "printf 'executed\\n' >> executions; printf retained-result";
    let arguments = json!({ "command": command });
    response(vec![tool_call(0, "append-once", "shell", arguments)])
}

async fn session_with_max_output(
    max_output: u64,
    responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
) -> (tempfile::TempDir, Requests, SessionHandle) {
    let root = tempfile::tempdir().unwrap();
    let requests = Requests::default();
    let provider = scripted_provider(&requests, responses);
    let profile = ModelProfile::new("test", "test", None, 128_000, max_output, false);
    let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
        .model_profile("test", profile)
        .build()
        .await
        .unwrap();
    (root, requests, harness.new_session().await.unwrap())
}

#[tokio::test]
async fn high_usage_tool_response_closes_exchange_before_summary_and_next_normal_request() {
    let (root, requests, session) = scripted_session([
        with_usage(shell_response(), &[threshold_usage()]),
        answer(summary_json().to_string()),
        answer("original final"),
    ])
    .await;
    // Enough old material to remove, but nowhere near the automatic threshold.
    seed_history(&session, 6_000).await;
    let found = session.prompt("Run the tool.").await.unwrap();
    assert_eq!(found, "original final");
    let executions = fs::read_to_string(root.path().join("executions")).await;
    // compaction must not re-execute the tool
    assert_eq!(executions.unwrap(), "executed\n");
    let records = session.runtime.store.records().await;
    use ModelPurpose::{Agent, Compaction};
    assert_eq!(purposes(&records), vec![Agent, Compaction, Agent]);
    let position = |matches: &dyn Fn(&SessionEvent) -> bool| {
        records.iter().position(|r| matches(&r.event)).unwrap()
    };
    let tool = position(&|event| {
        matches!(event, SessionEvent::MessageCommitted { message: Message::Tool(results) }
        if results.iter().any(|result| result.call_id == "append-once"))
    });
    let SessionEvent::MessageCommitted {
        message: tool_message,
    } = &records[tool].event
    else {
        unreachable!()
    };
    let Message::Tool(results) = tool_message else {
        unreachable!()
    };
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(results[0].result.to_string().contains("retained-result"));
    let summary = position(
        &|event| matches!(event, SessionEvent::ModelRequested { purpose, .. } if *purpose == Compaction),
    );
    let checkpoint = position(&|event| matches!(event, SessionEvent::Compaction { .. }));
    let next = records.iter().rposition(|record| {
        matches!(&record.event, SessionEvent::ModelRequested { purpose, .. } if *purpose == Agent)
    });
    assert!(tool < summary && summary < checkpoint && checkpoint < next.unwrap());
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
                .messages()
                .position(|message| message == tool_message)
                .expect("summary and continuation retain the actual tool result");
            assert!(
                matches!(&request.history[index - 1], Message::Assistant(items)
                if items.iter().flat_map(|item| &item.blocks).any(|block|
                    matches!(&block.content, BlockContent::ToolCall(call) if call.id() == "append-once")))
            );
        }
    }
    shutdown_session(session).await;
}

#[tokio::test]
async fn only_compaction_summaries_end_their_history() {
    use crate::provider::protocol::{HistoryLifetime::*, UserContent};
    let shell = |id| response(vec![tool_call(0, id, "shell", json!({"command": "true"}))]);
    let (_root, _requests, session) = scripted_session([
        shell("first"),
        with_usage(shell("second"), &[threshold_usage()]),
        answer(summary_json().to_string()),
        answer("done"),
    ])
    .await;
    seed_history(&session, 6_000).await;
    assert_eq!(session.prompt("Run both tools.").await.unwrap(), "done");
    let records = session.runtime.store.records().await;
    let requests = events!(&records, SessionEvent::ModelRequested { tail, history_lifetime, purpose, .. }
        => (*purpose, tail.as_slice(), *history_lifetime));
    let [first, second, summary, after] = requests.as_slice() else {
        panic!("unexpected requests: {requests:?}")
    };
    assert_eq!((summary.0, summary.2), (ModelPurpose::Compaction, Ending));
    assert_eq!(summary.1.last(), Some(&compaction::directive()));
    for (purpose, tail, lifetime) in [first, second, after] {
        assert_eq!((*purpose, *lifetime), (ModelPurpose::Agent, Continuing));
        assert!(matches!(tail, [Message::User(blocks)]
            if matches!(blocks.as_slice(), [UserContent::Runtime { .. }])));
    }
    shutdown_session(session).await;
}

#[tokio::test]
async fn high_usage_final_response_compacts_before_returning_original_text_regardless_of_max_output()
 {
    for max_output in [1, 120_000] {
        let final_answer = with_usage(answer("original final"), &[threshold_usage()]);
        let responses = [final_answer, answer(summary_json().to_string())];
        let (_root, requests, session) = session_with_max_output(max_output, responses).await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Finish.").await.unwrap(), "original final");
        // Live state must already show the reduced context, without further catch-up.
        let observation = session.runtime.events.observe().snapshot;
        let records = session.runtime.store.records().await;
        let found = purposes(&records);
        assert_eq!(found, vec![ModelPurpose::Agent, ModelPurpose::Compaction]);
        let checkpoint = events!(&records, SessionEvent::Compaction { checkpoint } => checkpoint);
        let checkpoint = checkpoint[0];
        let context = observation.context.get(&session.root).unwrap();
        assert!(checkpoint.after_tokens < checkpoint.before_tokens);
        // final-response compaction must refresh live occupancy before returning
        assert_eq!(context.tokens, checkpoint.after_tokens);
        assert_eq!(context.capacity, 128_000);
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 2);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].messages().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("answer", 0, "original final")])));
        }
        shutdown_session(session).await;
    }
}

#[tokio::test]
async fn oversized_history_or_tool_result_with_low_completed_usage_does_not_compact() {
    for tool_result in [false, true] {
        let low = with_usage(answer("done"), &[usage(10, 5, 1)]);
        let (_root, requests, session) = scripted_session([low]).await;
        if tool_result {
            seed_history(&session, 6_000).await;
            // An oversized historical tool result must not substitute for actual usage.
            let read = json!({"path": "research.txt"});
            let call = Message::Assistant(vec![tool_call(0, "large-result", "read", read)]);
            let result = Message::Tool(vec![ToolResult {
                call_id: "large-result".into(),
                name: "read".into(),
                result: json!({"content": "research ".repeat(100_000)}),
                images: vec![],
                is_error: false,
            }]);
            for message in [call, result] {
                session
                    .runtime
                    .commit(&session.root, message)
                    .await
                    .unwrap();
            }
        } else {
            seed_history(&session, 100_000).await;
        }
        assert_eq!(session.prompt("Use the result.").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let estimate = super::super::compaction::estimate_request(&requests.lock().unwrap()[0]);
        assert!(estimate > 128_000);
        shutdown_session(session).await;
    }
}

#[tokio::test]
async fn cumulative_usage_snapshots_and_separate_responses_are_not_added_for_compaction() {
    let snapshots = [usage(50_000, 0, 0), usage(50_000, 5_000, 5_000)];
    let responses = [
        with_usage(shell_response(), &snapshots),
        with_usage(answer("done"), &snapshots),
    ];
    let (_root, _, session) = session_with_max_output(120_000, responses).await;
    seed_history(&session, 6_000).await;
    assert_eq!(session.prompt("Run then finish.").await.unwrap(), "done");
    let records = session.runtime.store.records().await;
    let found = purposes(&records);
    assert_eq!(found, vec![ModelPurpose::Agent, ModelPurpose::Agent]);
    assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
    let usage = events!(&records, SessionEvent::Usage { usage, .. } => *usage);
    assert_eq!(usage, vec![snapshots[1], snapshots[1]]);
    shutdown_session(session).await;
}
