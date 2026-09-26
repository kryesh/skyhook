//! Summarizer response assembly and cancellation-safe usage accounting.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::provider::{
    ProviderContext, ProviderError,
    protocol::{LiveResponse, ModelRequest, Outcome, ResponseEvent, Step as LiveStep, Usage},
};
use crate::session::RequestSeq;
use futures_util::StreamExt;

impl SessionRuntime {
    pub(super) async fn summarize(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        request: ModelRequest,
        request_sequence: RequestSeq,
        model_attempt: &mut u64,
    ) -> Result<(compaction::Continuation, Usage), HarnessError> {
        let mut transient_attempt = 0u64;
        loop {
            if turn.cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            *model_attempt = model_attempt.saturating_add(1);
            let attempt = crate::session::AttemptRef {
                request: request_sequence,
                attempt: *model_attempt,
            };
            self.store
                .append(
                    turn.agent.clone(),
                    crate::session::SessionEvent::ModelAttemptStarted(attempt),
                )
                .await?;
            match self
                .summarize_attempt(turn, provider, request.clone(), request_sequence)
                .await
            {
                // The summary request stays frozen too, and transient failures do not
                // consume the separate validation budget in `compact_history`.
                Err(HarnessError::Provider(error)) => {
                    let recovered = self
                        .recover_model_failure(
                            turn,
                            (attempt, &mut transient_attempt),
                            Usage::default(),
                            error,
                        )
                        .await?;
                    if let Some(error) = recovered {
                        return Err(error.into());
                    }
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
        request_sequence: RequestSeq,
    ) -> Result<(compaction::Continuation, Usage), HarnessError> {
        let agent = turn.agent;
        let mut stream = provider.invoke(request);
        let mut live = LiveResponse::default();
        let (completion, usage) = loop {
            let event = tokio::select! {
                event = stream.next() => event,
                () = turn.cancellation.cancelled() => {
                    self.record_model_usage(agent, request_sequence, live.usage()).await?;
                    return Err(HarnessError::Interrupted);
                },
            };
            let checked = |event: ResponseEvent| event.checked().map_err(ProviderError::from);
            let event = match event.map(|event| event.and_then(checked)) {
                Some(Ok(event)) => event,
                Some(Err(error)) => {
                    if live.usage() != Usage::default() {
                        self.record_model_usage(agent, request_sequence, live.usage())
                            .await?;
                    }
                    return Err(error.into());
                }
                None => {
                    return Err(ProviderError::protocol("stream ended without completion").into());
                }
            };
            match live.push(event) {
                LiveStep::Open(open) => live = open,
                LiveStep::Ended {
                    completion, usage, ..
                } => break (completion, usage),
            }
        };

        self.record_model_usage(agent, request_sequence, usage)
            .await?;
        match completion.outcome() {
            Outcome::Answer => {}
            Outcome::Cut(_) => {
                return Err(HarnessError::Compaction(
                    "summarization was truncated; original history is retained".into(),
                ));
            }
            Outcome::ToolUse => {
                return Err(HarnessError::Compaction(
                    "summarizer returned a tool call; no tools were executed".into(),
                ));
            }
        }
        let text = crate::provider::protocol::visible_text(completion.items());
        let continuation = compaction::continuation(&text).map_err(HarnessError::Compaction)?;
        Ok((continuation, usage))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::{
        agent::runtime::HarnessError,
        session::{Message, SessionEvent},
    };
    use std::{sync::atomic::Ordering, time::Duration};
    use tokio_util::sync::CancellationToken;

    #[tokio::test(start_paused = true)]
    async fn summarizer_retries_frozen_requests_until_success() {
        for streaming in [false, true] {
            let failures = 4;
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
            let records = fixture.records().await;
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
            let retries = count!(&records, SessionEvent::ModelRecoveryScheduled { .. });
            assert_eq!(retries, failures);
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_do_not_consume_the_three_validation_attempts() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let before = fixture.provider.requests.lock().unwrap().len();
        let failures = &fixture.provider.summary_immediate_failures;
        failures.store(4, Ordering::SeqCst);
        *fixture.provider.summary.lock().unwrap() = "not a valid continuation".into();
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        assert_eq!(fixture.provider.requests.lock().unwrap().len() - before, 7);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_attempt() {
        let fixture = Fixture::new().await;
        let observed = usage(11, 7, 3);
        *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
        let provider = &fixture.provider;
        provider.agent_stream_failures.store(4, Ordering::SeqCst);
        assert!(fixture.session.prompt("Preserve usage.").await.is_ok());
        assert_eq!(fixture.session.usage().await, usage(44, 28, 12));
        fixture.add_history(20_000).await;
        let before = fixture.session.usage().await;
        provider.summary_stream_failures.store(4, Ordering::SeqCst);
        assert!(fixture.compact(&CancellationToken::new()).await.is_ok());
        let expected = usage(
            before.input_tokens + 44,
            before.cached_input_tokens + 28,
            before.output_tokens + 12,
        );
        assert_eq!(fixture.session.usage().await, expected);
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 8);
        let observed_usage =
            count!(&records, SessionEvent::Usage { usage, .. } if *usage == observed);
        assert_eq!(observed_usage, 8);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_after_four_summary_failures_preserves_history() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let failures = &fixture.provider.summary_stream_failures;
        failures.store(5, Ordering::SeqCst);
        let root = &fixture.session.root;
        let before = crate::session::project_history(&fixture.records().await, root);
        let cancellation = CancellationToken::new();
        let mut events = fixture.session.runtime.events.observe().updates;
        let (result, ()) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(fixture.compact(&cancellation), async {
                for _ in 0..4 {
                    while !matches!(events.recv().await.unwrap().event,
                        crate::agent::runtime::RuntimeEvent::Record(record)
                            if matches!(record.event, SessionEvent::ModelRecoveryScheduled { .. }))
                    {
                    }
                }
                cancellation.cancel();
            })
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        let records = fixture.records().await;
        let found = crate::session::project_history(&records, root);
        assert_eq!(found, before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_journals_observed_usage_once_without_committing_or_executing_tools() {
        for summary in [false, true] {
            let fixture = Fixture::new().await;
            let observed = usage(11, 7, 3);
            *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
            let pause = &fixture.provider.pause_stream_after_usage;
            pause.store(true, Ordering::SeqCst);
            let started = fixture.provider.started.notified();
            if summary {
                fixture.add_history(20_000).await;
                let cancellation = CancellationToken::new();
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(fixture.compact(&cancellation), async {
                        started.await;
                        cancellation.cancel();
                    })
                })
                .await
                .unwrap();
                assert!(matches!(result, Err(HarnessError::Interrupted)));
            } else {
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(fixture.session.prompt("Interrupt this turn."), async {
                        started.await;
                        fixture.session.interrupt().await;
                    })
                })
                .await
                .unwrap();
                assert!(result.is_err());
            }
            let records = fixture.records().await;
            let mut requested = records.iter().rev();
            let requested =
                requested.find(|r| matches!(r.event, SessionEvent::ModelRequested { .. }));
            let requested = requested.unwrap().sequence.request();
            let observed_events = events!(&records, SessionEvent::Usage { request, usage } if *request == requested => *usage);
            assert_eq!(observed_events, vec![observed]);
            assert_eq!(fixture.session.usage().await, observed);
            let committed = count!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(items) }
                if items.iter().any(|item| item.id().as_str() == "interrupted-tool"));
            assert_eq!(committed, 0);
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }
}
