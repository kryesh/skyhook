//! Summarizer response assembly and cancellation-safe usage accounting.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::provider::{
    ProviderContext,
    protocol::{BlockContent, ModelRequest, ResponseChunk, StopReason, Usage},
};
use futures_util::StreamExt;

impl SessionRuntime {
    pub(super) async fn summarize(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        request: ModelRequest,
        request_sequence: u64,
    ) -> Result<compaction::Continuation, HarnessError> {
        let agent = turn.agent;
        let mut stream = tokio::select! {
            result = provider.invoke(request) => result?,
            () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
        };
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let mut usage = Usage::default();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = turn.cancellation.cancelled() => {
                    self.record_model_usage(agent, request_sequence, usage).await?;
                    return Err(HarnessError::Interrupted);
                },
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = match chunk.and_then(|chunk| {
                assembler.push(&chunk)?;
                Ok(chunk)
            }) {
                Ok(chunk) => chunk,
                Err(error) => {
                    if usage != Usage::default() {
                        self.record_model_usage(agent, request_sequence, usage)
                            .await?;
                    }
                    return Err(error.into());
                }
            };
            if let ResponseChunk::UsageUpdated { usage: value } = chunk {
                usage = value;
            }
        }

        self.record_model_usage(agent, request_sequence, usage)
            .await?;
        let (blocks, _, reason) = assembler.finish()?;
        if matches!(
            reason,
            StopReason::MaxTokens | StopReason::ContentFilter | StopReason::Aborted
        ) {
            return Err(HarnessError::Compaction(
                "summarization was truncated; original history is retained".into(),
            ));
        }
        if blocks
            .iter()
            .flat_map(|item| &item.blocks)
            .any(|block| matches!(&block.content, BlockContent::ToolCall(_)))
        {
            return Err(HarnessError::Compaction(
                "summarizer returned a tool call; no tools were executed".into(),
            ));
        }
        let text: String = blocks
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        compaction::continuation(&text).map_err(HarnessError::Compaction)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::{
        agent::runtime::HarnessError,
        provider::protocol::{Message, Usage},
        session::SessionEvent,
    };
    use std::{sync::atomic::Ordering, time::Duration};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn summarizer_does_not_replay_provider_failures() {
        for streaming in [false, true] {
            let fixture = Fixture::new().await;
            let runtime = &fixture.session.runtime;

            fixture.add_history(20_000).await;
            let counter = if streaming {
                &fixture.provider.summary_stream_failures
            } else {
                &fixture.provider.summary_immediate_failures
            };
            counter.store(2, Ordering::SeqCst);
            assert!(fixture.compact(&CancellationToken::new()).await.is_err());
            assert_eq!(fixture.provider.requests.lock().unwrap().len(), 2);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            let records = runtime.store.records().await;
            assert!(
                !records
                    .iter()
                    .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
            );
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_request() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;

        let observed = Usage {
            input_tokens: 11,
            cached_input_tokens: 7,
            output_tokens: 3,
        };
        *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
        fixture
            .provider
            .agent_stream_failures
            .store(1, Ordering::SeqCst);
        assert!(
            fixture
                .session
                .prompt("Preserve failed usage.")
                .await
                .is_err()
        );
        assert_eq!(fixture.session.usage().await, observed);

        fixture.add_history(20_000).await;
        fixture
            .provider
            .summary_stream_failures
            .store(2, Ordering::SeqCst);
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        assert_eq!(
            fixture.session.usage().await,
            Usage {
                input_tokens: 22,
                cached_input_tokens: 14,
                output_tokens: 6
            }
        );
        let records = runtime.store.records().await;
        let failed: Vec<_> = records
            .iter()
            .filter_map(|record| match record.event {
                SessionEvent::ModelFailed { request, .. } => Some(request),
                SessionEvent::CompactionFailed {
                    request: Some(request),
                    ..
                } => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(failed.len(), 2);
        for request in failed {
            let recorded: Vec<_> = records
                .iter()
                .filter_map(|record| match record.event {
                    SessionEvent::Usage {
                        request: Some(sequence),
                        usage,
                    } if sequence == request => Some(usage),
                    _ => None,
                })
                .collect();
            assert_eq!(recorded, vec![observed]);
        }
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_journals_observed_usage_once_without_committing_or_executing_tools() {
        for summary in [false, true] {
            let fixture = Fixture::new().await;
            let runtime = &fixture.session.runtime;

            let observed = Usage {
                input_tokens: 11,
                cached_input_tokens: 7,
                output_tokens: 3,
            };
            *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
            fixture
                .provider
                .pause_stream_after_usage
                .store(true, Ordering::SeqCst);
            if summary {
                fixture.add_history(20_000).await;
                let cancellation = CancellationToken::new();
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(fixture.compact(&cancellation), async {
                        fixture.provider.started.notified().await;
                        cancellation.cancel();
                    })
                })
                .await
                .unwrap();
                assert!(matches!(result, Err(HarnessError::Interrupted)));
            } else {
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(fixture.session.prompt("Interrupt this turn."), async {
                        fixture.provider.started.notified().await;
                        fixture.session.interrupt().await;
                    })
                })
                .await
                .unwrap();
                assert!(result.is_err());
            }
            let records = runtime.store.records().await;
            let requested = records
                .iter()
                .rev()
                .find(|r| matches!(r.event, SessionEvent::ModelRequested { .. }))
                .unwrap()
                .sequence;
            let observed_events: Vec<_> = records
                .iter()
                .filter_map(|r| match r.event {
                    SessionEvent::Usage {
                        request: Some(request),
                        usage,
                    } if request == requested => Some(usage),
                    _ => None,
                })
                .collect();
            assert_eq!(observed_events, vec![observed]);
            assert_eq!(fixture.session.usage().await, observed);
            assert!(!records.iter().any(|r| matches!(&r.event,
                SessionEvent::MessageCommitted { message: Message::Assistant(items) }
                    if items.iter().any(|item| item.id == "interrupted-tool"))));
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }
}
