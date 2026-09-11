//! One agent's context window and independently owned provider conversation.

use std::collections::HashMap;

use super::{HarnessError, compact::TokenMeter};
use crate::{
    agent::ContextUsage,
    identity::AgentId,
    provider::{
        Provider, ProviderContext,
        profile::ModelProfile,
        protocol::{Message, ModelRequest, UserContent},
    },
    session::{EventRecord, SessionEvent, project_history},
};

pub(super) struct AgentContext {
    pub profile: ModelProfile,
    pub template: ModelRequest,
    pub projected: Vec<(u64, Message)>,
    pub meter: TokenMeter,
    pub provider: Box<dyn ProviderContext>,
    checkpoint: Option<u64>,
}

impl AgentContext {
    pub fn open(
        agent: &AgentId,
        profile: ModelProfile,
        template: ModelRequest,
        factory: &dyn Provider,
        records: &[EventRecord],
        restore_meter: bool,
    ) -> Result<Self, HarnessError> {
        let projected = project_history(records, agent)?;
        let provider = factory.open_context(agent.to_string())?;
        let meter = if restore_meter {
            TokenMeter::restore(records, agent, &template)
        } else {
            TokenMeter::default()
        };
        Ok(Self {
            profile,
            template,
            projected,
            meter,
            provider,
            checkpoint: checkpoint(records, agent),
        })
    }

    /// The journal remains authoritative, including compactions committed externally.
    pub fn refresh(
        &mut self,
        records: &[EventRecord],
        agent: &AgentId,
    ) -> Result<(), HarnessError> {
        let projected = project_history(records, agent)?;
        let checkpoint = checkpoint(records, agent);
        if self.checkpoint != checkpoint {
            self.meter = TokenMeter::default();
            self.checkpoint = checkpoint;
        }
        self.projected = projected;
        Ok(())
    }

    pub fn request(&self, runtime: UserContent) -> ModelRequest {
        let mut request = self.template.clone();
        request.messages = self
            .projected
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        request.messages.push(Message::User(vec![runtime]));
        request
    }

    pub fn needs_compaction(&self, request: &ModelRequest) -> bool {
        self.meter.estimate(request)
            >= self
                .profile
                .max_context
                .saturating_sub(self.profile.max_output)
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .iter()
            .any(|(_, message)| super::contains_images(std::slice::from_ref(message)))
    }
}

fn checkpoint(records: &[EventRecord], agent: &AgentId) -> Option<u64> {
    records
        .iter()
        .rev()
        .find(|record| {
            &record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. })
        })
        .map(|record| record.sequence)
}

/// Reconstruct context occupancy for historical agents without consulting current config.
pub(in crate::agent) fn recorded_context(
    records: &[EventRecord],
) -> HashMap<AgentId, ContextUsage> {
    let mut contexts = HashMap::new();
    let capacities: HashMap<_, _> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::AgentStarted {
                max_context: Some(capacity),
                ..
            }
            | SessionEvent::ModelChanged {
                max_context: capacity,
                ..
            } => Some((&record.agent, *capacity)),
            _ => None,
        })
        .collect();
    for (agent, capacity) in capacities {
        let Some(mut request) = records.iter().rev().find_map(|record| {
            if &record.agent == agent
                && matches!(
                    record.event,
                    SessionEvent::ModelRequested {
                        purpose: crate::session::ModelPurpose::Agent,
                        ..
                    }
                )
            {
                crate::session::reconstruct_model_request(records, record.sequence)
                    .ok()
                    .map(|(_, request)| request)
            } else {
                None
            }
        }) else {
            continue;
        };
        let runtime = request.messages.pop().filter(|message| {
            matches!(message, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserContent::Runtime { .. })))
        });
        request.messages.clear();
        let meter = TokenMeter::restore(records, agent, &request);
        let mut current = request;
        let Ok(history) = crate::session::project_history(records, agent) else {
            continue;
        };
        current.messages = history.into_iter().map(|(_, message)| message).collect();
        if let Some(message) = runtime {
            current.messages.push(message);
        }
        contexts.insert(
            agent.clone(),
            ContextUsage {
                tokens: meter.estimate(&current),
                capacity,
            },
        );
    }
    contexts
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::{Notify, Semaphore};

    use super::super::*;
    use crate::provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{StopReason, events_for_content},
    };

    struct Tracking {
        next: AtomicUsize,
        opened: Mutex<Vec<(usize, String)>>,
        dropped: Mutex<Vec<usize>>,
        fail_all_calls: AtomicBool,
        entered: Notify,
        released: Notify,
        gate: Semaphore,
    }

    impl Default for Tracking {
        fn default() -> Self {
            Self {
                next: AtomicUsize::new(0),
                opened: Mutex::default(),
                dropped: Mutex::default(),
                fail_all_calls: AtomicBool::new(false),
                entered: Notify::new(),
                released: Notify::new(),
                gate: Semaphore::new(0),
            }
        }
    }

    struct Factory(Arc<Tracking>);
    struct Context {
        tracking: Arc<Tracking>,
        id: usize,
    }

    impl Provider for Factory {
        fn open_context(
            &self,
            correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            let id = self.0.next.fetch_add(1, Ordering::SeqCst);
            self.0
                .opened
                .lock()
                .unwrap()
                .push((id, correlation.clone()));
            Ok(Box::new(Context {
                tracking: self.0.clone(),
                id,
            }))
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            self.tracking.dropped.lock().unwrap().push(self.id);
            self.tracking.released.notify_one();
        }
    }

    impl ProviderContext for Context {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            let latest = request
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User(blocks) => blocks.iter().find_map(|block| match block {
                        UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    }),
                    _ => None,
                });
            let block = latest == Some("block");
            let tracking = self.tracking.clone();
            Box::pin(async move {
                if tracking.fail_all_calls.load(Ordering::SeqCst) {
                    return Err(ProviderError::protocol("retry this request"));
                }
                if block {
                    tracking.entered.notify_one();
                    tracking.gate.acquire().await.unwrap().forget();
                }
                let item = AssistantContent::text("text/0", 0, "done");
                let mut events = events_for_content(&[item]);
                events.push(ResponseChunk::ResponseEnded {
                    stop_reason: StopReason::EndTurn,
                });
                Ok(
                    Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                        as ResponseStream,
                )
            })
        }
    }

    async fn child_job(session: &SessionHandle) -> JobId {
        session
            .runtime
            .jobs
            .create(crate::job::JobSpec::test(session.root.clone(), "agent"))
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn cancelled_and_failed_children_release_only_their_own_context() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Arc::new(Tracking::default());
        let harness = harness(root.path(), tracking.clone()).await;
        let session = harness.new_session().await.unwrap();
        for (index, fail) in [(1, false), (2, true)] {
            let child = session.root.child(index);
            let owner_job = child_job(&session).await;
            let sender = session
                .runtime
                .spawn_agent(AgentLaunch {
                    id: child.clone(),
                    owner_job: Some(owner_job),
                    model_profile: "first".into(),
                    todos: None,
                    available_depth: 0,
                    location: crate::execution::ExecutionLocation::root(root.path().to_owned()),
                })
                .await
                .unwrap();
            tracking.fail_all_calls.store(fail, Ordering::SeqCst);
            let (done, received) = oneshot::channel();
            sender
                .send(AgentCommand::Input {
                    model: None,
                    content: vec![UserContent::Text {
                        text: "block".into(),
                    }],
                    done: Some(done),
                })
                .await
                .unwrap();
            if !fail {
                tracking.entered.notified().await;
                session.runtime.interrupt_tree(&child).await;
            }
            assert!(received.await.unwrap().is_err());
            wait_dropped(&tracking, index as usize).await;
            tracking.fail_all_calls.store(false, Ordering::SeqCst);
            session.prompt("root still usable").await.unwrap();
            assert!(!tracking.dropped.lock().unwrap().contains(&0));
        }
        assert_eq!(tracking.opened.lock().unwrap().len(), 3);
        session.shutdown().await.unwrap();
        wait_dropped(&tracking, 0).await;
    }

    fn profile(model: &str) -> ModelProfile {
        ModelProfile {
            provider: "test".into(),
            model: model.into(),
            reasoning: None,
            max_context: 128_000,
            max_output: 16_384,
            supports_images: false,
        }
    }

    async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
        HarnessBuilder::new(root)
            .session_root(root.join("sessions"))
            .provider("test", Arc::new(Factory(tracking)))
            .model_profile("first", profile("first"))
            .default_model_profile("first")
            .build()
            .await
            .unwrap()
    }

    async fn wait_dropped(tracking: &Tracking, id: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = tracking.released.notified();
                if tracking.dropped.lock().unwrap().contains(&id) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("context released");
    }
}
