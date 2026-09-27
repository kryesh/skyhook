//! Summarizer response assembly and cancellation-safe usage accounting.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::agent::CompactionError;
use crate::provider::{
    ProviderContext,
    protocol::{Completion, CutReason, ModelRequest, Outcome, Usage, visible_text},
};
use crate::session::{AttemptRef, RequestSeq, SessionEvent};

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
            let attempt = AttemptRef {
                request: request_sequence,
                attempt: *model_attempt,
            };
            let started = SessionEvent::ModelAttemptStarted(attempt);
            self.store.append(turn.agent.clone(), started).await?;
            let stream = provider.invoke(request.clone());
            match self.consume_stream(turn, attempt, stream, |_| {}).await? {
                Ok((completion, usage)) => {
                    self.record_model_usage(turn.agent, request_sequence, usage)
                        .await?;
                    return Ok((continuation(&completion)?, usage));
                }
                // The summary request stays frozen too, and transient failures do not
                // consume the separate validation budget in `compact_history`.
                Err((error, usage)) => {
                    let attempt = (attempt, &mut transient_attempt);
                    let recovered = self.recover_model_failure(turn, attempt, usage, error);
                    if let Some(error) = recovered.await? {
                        return Err(error.into());
                    }
                }
            }
        }
    }
}

/// Only a cut-off summary is a truncated one, which a fresh summary may complete. A
/// refusal is deterministic for the request and an abort is the provider's own
/// failure: both fail the round as what they are.
fn continuation(completion: &Completion) -> Result<compaction::Continuation, HarnessError> {
    let text = visible_text(completion.items());
    match completion.outcome() {
        Outcome::Answer => {}
        Outcome::Cut(CutReason::Refusal) => {
            return Err(HarnessError::Refused(super::super::turn::refusal_detail(
                &text,
            )));
        }
        Outcome::Cut(CutReason::Aborted) => return Err(HarnessError::ProviderAborted),
        Outcome::Cut(CutReason::MaxTokens | CutReason::Incomplete) => {
            return Err(CompactionError::Truncated.into());
        }
        Outcome::ToolUse => return Err(CompactionError::ToolCall.into()),
    }
    Ok(compaction::continuation(&text)?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::agent::runtime::tests::{journaled_usage, next_recovery};
    use crate::{
        agent::runtime::HarnessError,
        session::{Message, RequestPhase, SessionEvent},
    };
    use tokio_util::sync::CancellationToken;

    /// A cut-off summary is retried as truncated; a refused or aborted one fails the
    /// round at once, diagnosed as what it was.
    #[tokio::test(start_paused = true)]
    async fn only_a_cut_off_summary_is_retried_as_truncated() {
        use crate::agent::{CompactionFault, FailureKind};
        use crate::provider::protocol::{AssistantItem, CutReason};
        let cases = [
            (CutReason::MaxTokens, 3, CompactionFault::Truncated),
            (CutReason::Incomplete, 3, CompactionFault::Truncated),
            (
                CutReason::Refusal,
                1,
                CompactionFault::Summary(FailureKind::Refused),
            ),
            (
                CutReason::Aborted,
                1,
                CompactionFault::Summary(FailureKind::Aborted),
            ),
        ];
        for (reason, attempts, fault) in cases {
            let partial = || vec![AssistantItem::text("text/0", 0, "partial")];
            let steps = (0..3).map(|_| Step::new(cut(partial(), reason)));
            let fixture = Fixture::new(steps).await;
            fixture.add_history(20_000).await;
            let error = fixture.compact(&CancellationToken::new()).await;
            assert_eq!(CompactionFault::from(&error.unwrap_err()), fault);
            assert_eq!(fixture.requests().len() - 1, attempts, "{reason:?}");
            let records = fixture.records().await;
            let faults =
                events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
            assert_eq!(faults, vec![fault; attempts]);
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn summarizer_retries_frozen_requests_until_success() {
        for streaming in [false, true] {
            let failures = 4;
            let failure = || match streaming {
                true => failing_stream(None),
                false => Step::fail(recoverable()),
            };
            let steps = (0..failures).map(|_| failure());
            let fixture = Fixture::new(steps.chain([Step::new(summary(summary_json()))])).await;
            fixture.add_history(20_000).await;
            fixture.compact(&CancellationToken::new()).await.unwrap();
            let requests = fixture.requests();
            let summaries = &requests[1..];
            assert_eq!(summaries.len(), failures + 1);
            assert!(summaries.windows(2).all(|pair| pair[0] == pair[1]));
            assert_eq!(fixture.script.remaining(), 0);
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
        let failures = (0..4).map(|_| Step::fail(recoverable()));
        let invalid = (0..3).map(|_| Step::new(summary("not a valid continuation")));
        let fixture = Fixture::new(failures.chain(invalid)).await;
        fixture.add_history(20_000).await;
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        assert_eq!(fixture.requests().len() - 1, 7);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_attempt() {
        let observed = usage(11, 7, 3);
        let failures = || (0..4).map(|_| failing_stream(Some(observed)));
        let steps = failures()
            .chain([Step::new(answer("done"))])
            .chain(failures())
            .chain([Step::new(summary(summary_json()))]);
        let fixture = Fixture::new(steps).await;
        assert!(fixture.session.prompt("Preserve usage.").await.is_ok());
        assert_eq!(journaled_usage(&fixture.session).await, usage(44, 28, 12));
        fixture.add_history(20_000).await;
        let before = journaled_usage(&fixture.session).await;
        assert!(fixture.compact(&CancellationToken::new()).await.is_ok());
        let expected = usage(
            before.input_tokens + 44,
            before.cached_input_tokens + 28,
            before.output_tokens + 12,
        );
        assert_eq!(journaled_usage(&fixture.session).await, expected);
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
        let fixture = Fixture::new((0..5).map(|_| failing_stream(None))).await;
        fixture.add_history(20_000).await;
        let root = &fixture.session.root;
        let before = crate::session::project_history(&fixture.records().await, root);
        let cancellation = CancellationToken::new();
        let mut events = fixture.session.runtime.events.observe().updates;
        let (result, ()) = bounded(async {
            tokio::join!(fixture.compact(&cancellation), async {
                for _ in 0..4 {
                    next_recovery(&mut events).await;
                }
                cancellation.cancel();
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_eq!(fixture.script.remaining(), 1);
        let records = fixture.records().await;
        let found = crate::session::project_history(&records, root);
        assert_eq!(found, before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        // The interruption settles the summary request, whether during a retry's
        // backoff or before its first attempt.
        let summary_phase = async || {
            let ledger = fixture.session.observe().await.snapshot.ledger;
            let summary = ledger.latest(root).and_then(|request| ledger.get(request));
            summary.unwrap().phase.clone()
        };
        let phase = summary_phase().await;
        assert!(matches!(
            phase,
            RequestPhase::Interrupted {
                attempt: Some(4),
                ..
            }
        ));
        let result = fixture.compact(&cancellation).await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        let phase = summary_phase().await;
        assert!(matches!(
            phase,
            RequestPhase::Interrupted { attempt: None, .. }
        ));
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_journals_observed_usage_once_without_committing_or_executing_tools() {
        for summary in [false, true] {
            let observed = usage(11, 7, 3);
            let fixture = Fixture::new([held_after(observed)]).await;
            if summary {
                fixture.add_history(20_000).await;
                let cancellation = CancellationToken::new();
                let (result, ()) = bounded(async {
                    tokio::join!(fixture.compact(&cancellation), async {
                        fixture.script.held(1).await;
                        cancellation.cancel();
                    })
                })
                .await;
                assert!(matches!(result, Err(HarnessError::Interrupted)));
            } else {
                let (result, ()) = bounded(async {
                    tokio::join!(fixture.session.prompt("Interrupt this turn."), async {
                        fixture.script.held(1).await;
                        fixture.session.interrupt().await;
                    })
                })
                .await;
                assert!(result.is_err());
            }
            let records = fixture.records().await;
            let mut requested = records.iter().rev();
            let requested =
                requested.find(|r| matches!(r.event, SessionEvent::ModelRequested { .. }));
            let requested = requested.unwrap().sequence.request();
            let observed_events = events!(&records, SessionEvent::Usage { request, usage } if *request == requested => *usage);
            assert_eq!(observed_events, vec![observed]);
            let ledger = fixture.session.observe().await.snapshot.ledger;
            let phase = &ledger.get(requested).unwrap().phase;
            let at = phase.settled_at().unwrap();
            assert_eq!(
                *phase,
                RequestPhase::Interrupted {
                    attempt: Some(1),
                    at
                }
            );
            assert_eq!(journaled_usage(&fixture.session).await, observed);
            let committed = count!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(items) }
                if items.iter().any(|item| item.id().as_str() == "interrupted-tool"));
            assert_eq!(committed, 0);
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }
}
