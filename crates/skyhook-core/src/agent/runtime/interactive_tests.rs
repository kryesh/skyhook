//! Interactive is a root-host boundary, not a child-to-parent messaging gate.

use std::{future::Future, sync::Mutex as StdMutex, time::Duration};

use super::*;
use crate::{
    agent::{Question, QuestionFuture},
    job::JobState,
    provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{ItemKind, StopReason, events_for_content},
    },
    tool::executor::ExecutionError,
};

#[derive(Clone, Default)]
struct RecordingProvider {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    responses: Arc<StdMutex<VecDeque<Vec<AssistantContent>>>>,
}

impl Provider for RecordingProvider {
    fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(self.clone()))
    }
}

impl ProviderContext for RecordingProvider {
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
        self.requests.lock().unwrap().push(request);
        let content = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![AssistantContent::text("answer", 0, "done")]);
        let reason = if content.iter().any(|item| item.kind == ItemKind::ToolCall) {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let mut events = events_for_content(&content);
        events.push(ResponseChunk::ResponseEnded {
            stop_reason: reason,
        });
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter(events.into_iter().map(Ok))) as ResponseStream)
        })
    }
}

#[derive(Default)]
struct RecordingQuestions(StdMutex<Vec<AgentId>>);

impl QuestionHandler for RecordingQuestions {
    fn ask(&self, agent: AgentId, _: Vec<Question>) -> QuestionFuture {
        self.0.lock().unwrap().push(agent);
        Box::pin(async { Ok(json!("host-answer")) })
    }
}

fn builder(root: &Path, provider: RecordingProvider, enabled: bool) -> HarnessBuilder {
    let mut capabilities = CapabilitySet::default();
    if !enabled {
        capabilities.remove(Capability::Interactive);
    }
    HarnessBuilder::new(root)
        .session_root(root.join("sessions"))
        .provider("test", Arc::new(provider))
        .model_profile(
            "test",
            ModelProfile {
                provider: "test".into(),
                model: "test".into(),
                reasoning: None,
                max_context: 128_000,
                max_output: 16_384,
                supports_images: false,
            },
        )
        .default_model_profile("test")
        .max_child_depth(1)
        .capabilities(capabilities)
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("interactive test stalled")
}

async fn terminal(session: &SessionHandle, job: JobId) -> crate::job::JobEnvelope {
    bounded(async {
        loop {
            if session
                .runtime
                .jobs
                .snapshot(job)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                return session.runtime.jobs.wait(job, None, true).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

#[tokio::test]
async fn root_interactive_surface_and_all_dispatch_paths_follow_the_gate() {
    for enabled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::default();
        let handler = Arc::new(RecordingQuestions::default());
        let harness = builder(root.path(), provider.clone(), enabled)
            .question_handler(handler.clone())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let executor = session
            .runtime
            .executor
            .clone()
            .with_capabilities(session.runtime.harness.capabilities.clone());
        assert_eq!(executor.surface().get("ask").is_some(), enabled);
        assert_eq!(
            executor
                .surface_for_agent(&session.root)
                .get("ask")
                .is_some(),
            enabled
        );
        assert_eq!(bounded(session.prompt("list tools")).await.unwrap(), "done");
        assert_eq!(
            provider.requests.lock().unwrap()[0]
                .tools
                .iter()
                .any(|tool| tool.name == "ask"),
            enabled
        );
        let binding = session.run_script("return typeof tool.ask;").await.unwrap();
        assert_eq!(
            binding.value["value"],
            if enabled { "function" } else { "undefined" }
        );

        // All invocation entrypoints reject before creating an ask job, including
        // the lower-level script dispatcher (not only the generated JS binding).
        for background in [false, true] {
            for route in ["host", "model", "script"] {
                let args = json!({"id":"root", "prompt":"question", "bg":background});
                let result = bounded(async {
                    match route {
                        "host" => {
                            executor
                                .execute(session.root.clone(), "ask", args, None)
                                .await
                        }
                        "model" => {
                            executor
                                .execute_model(session.root.clone(), "ask", args, None)
                                .await
                        }
                        _ => {
                            executor
                                .execute_script(session.root.clone(), "ask", args, None)
                                .await
                        }
                    }
                })
                .await;
                if enabled {
                    let result = result.unwrap();
                    let job = terminal(&session, result.job).await;
                    assert_eq!(job.state, JobState::Completed);
                    assert_eq!(job.output, Some(json!("host-answer")));
                } else {
                    assert!(result.unwrap_err().to_string().contains("unavailable"));
                }
            }
        }

        // Independently cover both a background script and a background ask.
        // Root jobs nested inside a script still have the root as their actor.
        for script_background in [false, true] {
            for ask_background in [false, true] {
                let source = format!(
                    "return await tool.ask({{id:'nested', prompt:'question', bg:{ask_background}}});"
                );
                let result = bounded(executor.execute(
                    session.root.clone(),
                    "script",
                    json!({"source":source, "bg":script_background}),
                    None,
                ))
                .await;
                if enabled {
                    let result = result.unwrap();
                    let job = terminal(&session, result.job).await;
                    assert_eq!(job.state, JobState::Completed);
                    if ask_background {
                        let ask: JobId = serde_json::from_value(
                            job.output.as_ref().unwrap()["value"]["id"].clone(),
                        )
                        .unwrap();
                        assert_eq!(
                            terminal(&session, ask).await.output,
                            Some(json!("host-answer"))
                        );
                    } else {
                        assert_eq!(job.output.as_ref().unwrap()["value"], "host-answer");
                    }
                } else if script_background {
                    let job = terminal(&session, result.unwrap().job).await;
                    assert_eq!(job.state, JobState::Failed);
                } else {
                    assert!(matches!(result, Err(ExecutionError::Failed { .. })));
                }
            }
        }
        let jobs = session.inspect_jobs(&session.root).await;
        let asks = jobs
            .iter()
            .filter(|job| job.tool == "ask")
            .collect::<Vec<_>>();
        assert_eq!(asks.len(), if enabled { 10 } else { 0 });
        for ask in asks {
            assert_eq!(
                terminal(&session, ask.id).await.output,
                Some(json!("host-answer"))
            );
        }
        let calls = handler.0.lock().unwrap().clone();
        assert_eq!(calls.len(), if enabled { 10 } else { 0 });
        assert!(calls.iter().all(|actor| actor == &session.root));
        tests::shutdown_session(session).await;
    }
}

#[tokio::test]
async fn disabled_root_background_ask_cannot_wait_without_a_handler() {
    let root = tempfile::tempdir().unwrap();
    let harness = builder(root.path(), RecordingProvider::default(), false)
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    let executor = session
        .runtime
        .executor
        .clone()
        .with_capabilities(session.runtime.harness.capabilities.clone());
    for background in [false, true] {
        assert!(
            bounded(executor.execute(
                session.root.clone(),
                "ask",
                json!({"id":"root", "prompt":"question", "bg":background}),
                None
            ))
            .await
            .is_err()
        );
    }
    assert!(session.inspect_jobs(&session.root).await.is_empty());
    tests::shutdown_session(session).await;
}

#[tokio::test]
async fn noninteractive_children_ask_and_receive_root_answers_without_host_interaction() {
    for scripted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::default();
        let question = if scripted {
            ToolCall {
                id: "question".into(),
                name: "script".into(),
                arguments: json!({
                    "source":"return await tool.ask({id:'child', prompt:'parent question'});"
                }),
            }
        } else {
            ToolCall {
                id: "question".into(),
                name: "ask".into(),
                arguments: json!({
                    "id":"child", "prompt":"parent question"
                }),
            }
        };
        provider
            .responses
            .lock()
            .unwrap()
            .push_back(vec![AssistantContent::tool_call("call", 0, question)]);
        let handler = Arc::new(RecordingQuestions::default());
        let harness = builder(root.path(), provider.clone(), false)
            .question_handler(handler.clone())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let output =
            bounded(session.run_script(
                "return await tool.agent({prompt:'ask the parent', depth:0, bg:true});",
            ))
            .await
            .unwrap();
        let owner: JobId = serde_json::from_value(output.value["value"]["id"].clone()).unwrap();
        bounded(async {
            loop {
                let job = session.runtime.jobs.snapshot(owner).await.unwrap();
                if job.state == JobState::WaitingInput {
                    assert_eq!(job.output.as_ref().unwrap()["questions"][0]["id"], "child");
                    break;
                }
                assert!(
                    !job.state.is_terminal(),
                    "child ended before asking: {job:?}"
                );
                tokio::task::yield_now().await;
            }
        })
        .await;
        let executor = session
            .runtime
            .executor
            .clone()
            .with_capabilities(session.runtime.harness.capabilities.clone());
        assert!(
            !executor
                .surface_for_agent(&session.root)
                .get("ask")
                .is_some()
        );
        let child = session.root.child(1);
        assert!(executor.surface_for_agent(&child).get("ask").is_some());
        assert!(
            !session
                .runtime
                .harness
                .capabilities
                .for_agent(0)
                .contains(Capability::Interactive)
        );
        assert!(
            executor
                .registry()
                .get("ask")
                .unwrap()
                .capabilities()
                .is_empty(),
            "child messaging must not require an Interactive permission"
        );
        bounded(session.run_script(format!(
            "return await tool.job({owner}).send({{value:'parent-answer'}});"
        )))
        .await
        .unwrap();
        assert_eq!(terminal(&session, owner).await.output, Some(json!("done")));
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.tools.iter().any(|tool| tool.name == "ask"))
            );
            assert!(
                serde_json::to_string(&requests[1].messages)
                    .unwrap()
                    .contains("parent-answer")
            );
        }
        assert!(
            handler.0.lock().unwrap().is_empty(),
            "child asks must never reach host handler"
        );
        tests::shutdown_session(session).await;
    }
}

#[tokio::test]
async fn question_coordinator_rechecks_root_gate_before_waiting_for_input() {
    let root = tempfile::tempdir().unwrap();
    let handler = Arc::new(RecordingQuestions::default());
    let harness = builder(root.path(), RecordingProvider::default(), false)
        .question_handler(handler.clone())
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    for background in [false, true] {
        let lease = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                accepts_input: true,
                background,
                ..crate::job::JobSpec::test(session.root.clone(), "ask")
            })
            .await
            .unwrap();
        let location = crate::execution::ExecutionLocation::root(root.path().to_owned());
        let context = crate::tool::ToolContext::new(
            crate::tool::authorization::AuthorizationSubject {
                agent: session.root.clone(),
                job: lease.id,
                parent: None,
                scope: None,
                capabilities: session.runtime.harness.capabilities.clone(),
                cancellation: lease.cancellation,
            },
            location.clone(),
            location,
            lease.input,
            session.runtime.jobs.clone(),
        );
        let error = bounded(session.runtime.questions.coordinate_question(
            context,
            Question {
                id: "bypass".into(),
                prompt: "question".into(),
                options: vec![],
            },
        ))
        .await
        .unwrap_err();
        assert!(matches!(error, crate::tool::ToolError::Denied(_)));
        assert_ne!(
            session.runtime.jobs.snapshot(lease.id).await.unwrap().state,
            JobState::WaitingInput
        );
        session.runtime.jobs.cancel(lease.id).await.unwrap();
    }
    assert!(handler.0.lock().unwrap().is_empty());
    tests::shutdown_session(session).await;
}
