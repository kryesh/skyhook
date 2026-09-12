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
        model_attempt: &mut u64,
    ) -> Result<compaction::Continuation, HarnessError> {
        let mut transient_attempt = 0u64;
        loop {
            if turn.cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            *model_attempt = model_attempt.saturating_add(1);
            self.store
                .append(
                    turn.agent.clone(),
                    crate::session::SessionEvent::ModelAttemptStarted {
                        request: request_sequence,
                        attempt: *model_attempt,
                    },
                )
                .await?;
            match self
                .summarize_attempt(turn, provider, request.clone(), request_sequence)
                .await
            {
                Err(HarnessError::Provider(error)) => {
                    self.record_model_failure(
                        turn.agent,
                        request_sequence,
                        *model_attempt,
                        Usage::default(),
                        error.to_string(),
                    )
                    .await?;
                    if error.recovery().is_none() {
                        return Err(error.into());
                    }
                    // Keep the summary request frozen too. Failed partial summaries
                    // never become a checkpoint. Transient failures do not consume
                    // the separate validation budget in compact_history.
                    transient_attempt = transient_attempt.saturating_add(1);
                    self.schedule_model_recovery(
                        turn,
                        request_sequence,
                        super::super::recovery::RecoveryAttempt {
                            model: *model_attempt,
                            transient: transient_attempt,
                        },
                        &error,
                        provider,
                    )
                    .await?;
                }
                result => return result,
            }
        }
    }

    async fn summarize_attempt(
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

    #[tokio::test(start_paused = true)]
    async fn summarizer_retries_frozen_requests_past_u8_limit_until_success() {
        for streaming in [false, true] {
            for failures in [4, 260] {
                let fixture = Fixture::new().await;
                fixture.add_history(20_000).await;
                let before = fixture.provider.requests.lock().unwrap().len();
                let counter = if streaming {
                    &fixture.provider.summary_stream_failures
                } else {
                    &fixture.provider.summary_immediate_failures
                };
                counter.store(failures, Ordering::SeqCst);
                fixture.compact(&CancellationToken::new()).await.unwrap();
                let requests = fixture.provider.requests.lock().unwrap().clone();
                let summaries = &requests[before..];
                assert_eq!(summaries.len(), failures + 1);
                assert!(summaries.windows(2).all(|pair| pair[0] == pair[1]));
                assert_eq!(counter.load(Ordering::SeqCst), 0);
                let records = fixture.session.runtime.store.records().await;
                assert!(
                    records
                        .iter()
                        .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
                );
                assert_eq!(
                    records
                        .iter()
                        .filter(|record| matches!(
                            record.event,
                            SessionEvent::ModelRecoveryScheduled { .. }
                        ))
                        .count(),
                    failures
                );
                fixture.assert_no_tool_execution().await;
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_do_not_consume_the_three_validation_attempts() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let before = fixture.provider.requests.lock().unwrap().len();
        fixture
            .provider
            .summary_immediate_failures
            .store(4, Ordering::SeqCst);
        *fixture.provider.summary.lock().unwrap() = "not a valid continuation".into();
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        assert_eq!(fixture.provider.requests.lock().unwrap().len() - before, 7);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_attempt() {
        let fixture = Fixture::new().await;
        let observed = Usage {
            input_tokens: 11,
            cached_input_tokens: 7,
            output_tokens: 3,
        };
        *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
        fixture
            .provider
            .agent_stream_failures
            .store(4, Ordering::SeqCst);
        assert!(
            fixture
                .session
                .prompt("Preserve failed usage.")
                .await
                .is_ok()
        );
        assert_eq!(
            fixture.session.usage().await,
            Usage {
                input_tokens: 44,
                cached_input_tokens: 28,
                output_tokens: 12
            }
        );
        fixture.add_history(20_000).await;
        let before = fixture.session.usage().await;
        fixture
            .provider
            .summary_stream_failures
            .store(4, Ordering::SeqCst);
        assert!(fixture.compact(&CancellationToken::new()).await.is_ok());
        assert_eq!(
            fixture.session.usage().await,
            Usage {
                input_tokens: before.input_tokens + 44,
                cached_input_tokens: before.cached_input_tokens + 28,
                output_tokens: before.output_tokens + 12,
            }
        );
        let records = fixture.session.runtime.store.records().await;
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                .count(),
            8
        );
        assert_eq!(records.iter().filter(|record| matches!(record.event, SessionEvent::Usage { usage, .. } if usage == observed)).count(), 8);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_after_four_summary_failures_preserves_history() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        fixture
            .provider
            .summary_stream_failures
            .store(5, Ordering::SeqCst);
        let before = crate::session::project_history(
            &fixture.session.runtime.store.records().await,
            &fixture.session.root,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut events = fixture.session.subscribe();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(fixture.compact(&cancellation), async {
                let mut retries = 0;
                loop {
                    if let crate::agent::runtime::RuntimeEvent::Record(record) =
                        events.recv().await.unwrap()
                        && matches!(record.event, SessionEvent::ModelRecoveryScheduled { .. })
                    {
                        retries += 1;
                        if retries == 4 {
                            cancellation.cancel();
                            break;
                        }
                    }
                }
            })
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_eq!(
            fixture
                .provider
                .summary_stream_failures
                .load(Ordering::SeqCst),
            1
        );
        let records = fixture.session.runtime.store.records().await;
        assert_eq!(
            crate::session::project_history(&records, &fixture.session.root).unwrap(),
            before
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        );
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
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
