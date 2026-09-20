//! Dispatch shim responses without blocking frame reads on host callbacks.
use super::permissions::rebase_remote_permissions;
use super::*;
use crate::tool::authorization::AuthorizationError;
use tokio::io::AsyncRead;

pub(super) async fn route_responses<R>(
    output: R,
    state: &Mutex<ConnectionState>,
    host: (&Arc<Mutex<RequestWriter>>, &AuthorizationCoordinator),
    target: String,
    prompts: Arc<dyn SensitivePromptHandler>,
) where
    R: AsyncRead + Unpin,
{
    let mut results = super::results::Results::default();
    let Err(error) = route(output, state, host, target, prompts, &mut results).await;
    {
        let mut state = state.lock().await;
        state.failure.get_or_insert_with(|| error.clone());
        state.streams.clear();
    }
    results.shutdown(state).await;
    fail_connection(state, error).await;
}

async fn route<R>(
    mut output: R,
    state: &Mutex<ConnectionState>,
    host: (&Arc<Mutex<RequestWriter>>, &AuthorizationCoordinator),
    target: String,
    prompts: Arc<dyn SensitivePromptHandler>,
    results: &mut super::results::Results,
) -> Result<std::convert::Infallible, RemoteError>
where
    R: AsyncRead + Unpin,
{
    let mut callbacks = tokio::task::JoinSet::<Result<Option<PromptId>, RemoteError>>::new();
    let mut prompt_tasks = HashMap::<PromptId, crate::job::CancellationToken>::new();
    loop {
        // Keep the frame read alive while servicing callbacks: read_exact is not
        // cancellation-safe, so restarting it could lose part of a frame.
        let response = {
            let reading = read_frame::<_, Response>(&mut output);
            tokio::pin!(reading);
            loop {
                tokio::select! {
                    response = &mut reading => break response,
                    completed = results.tasks.join_next(), if !results.tasks.is_empty() => {
                        results.complete(state, completed.expect("nonempty ingestion set")).await?;
                    }
                    completed = callbacks.join_next(), if !callbacks.is_empty() => {
                        match completed {
                            Some(Ok(Ok(Some(id)))) => { prompt_tasks.remove(&id); }
                            Some(Ok(Ok(None))) => {}
                            Some(Err(error)) if error.is_cancelled() => {}
                            Some(Ok(Err(error))) => return Err(error),
                            Some(Err(error)) => return Err(RemoteError::ConnectionTask(error.to_string())),
                            None => unreachable!("nonempty callback set"),
                        }
                    }
                }
            }
        };
        let response = response
            .map_err(RemoteError::io)?
            .ok_or_else(|| RemoteError::Protocol("shim closed before replying".to_owned()))?;
        match response {
            Response::Payload { request_id, event } => {
                results.payload(state, host.0, request_id, event).await?;
            }
            Response::SensitiveCancelled { prompt_id } => {
                if let Some(cancellation) = prompt_tasks.remove(&prompt_id) {
                    cancellation.cancel();
                }
            }
            Response::StreamData { channel, data } => {
                let invalid = data.len() > crate::remote::flow::CHUNK_BYTES;
                let overflow = {
                    let state = state.lock().await;
                    state.streams.get(&channel).is_some_and(|sender| {
                        !sender.output.is_closed() && sender.output.try_send(Ok(data)).is_err()
                    })
                };
                if invalid || overflow {
                    return Err(RemoteError::Protocol(
                        "invalid stream output or flow-control overflow".into(),
                    ));
                }
            }
            Response::StreamClosed { channel, error } => {
                if let Some(sender) = state.lock().await.streams.remove(&channel)
                    && let Some(error) = error
                {
                    let _ = sender.output.try_send(Err(RemoteError::Ssh(error)));
                }
            }
            Response::StreamAck { channel } => {
                let invalid = {
                    let state = state.lock().await;
                    state
                        .streams
                        .get(&channel)
                        .is_some_and(|stream| stream.credit.acknowledge().is_err())
                };
                if invalid {
                    return Err(RemoteError::Protocol("invalid stream credit".into()));
                }
            }
            Response::SensitivePrompt {
                prompt_id,
                mut prompt,
            } => {
                if prompt_tasks.contains_key(&prompt_id) {
                    return Err(RemoteError::Protocol("duplicate prompt".into()));
                }
                prompt.message = format!("[origin={target}] {}", prompt.message);
                let writer = host.0.clone();
                let prompts = prompts.clone();
                let cancellation = crate::job::CancellationToken::new();
                let cancelled = cancellation.clone();
                callbacks.spawn(async move {
                    // Cancellation stops the prompt, not a partially committed
                    // answer frame. Once answering starts only connection shutdown
                    // may abort this write.
                    let answer = tokio::select! {
                        answer = prompts.prompt(prompt) => answer,
                        () = cancelled.cancelled() => return Ok(Some(prompt_id)),
                    };
                    let answer = match answer {
                        Ok(value) => crate::remote::prompt::PromptAnswer::Accepted(value),
                        Err(_) => crate::remote::prompt::PromptAnswer::Rejected,
                    };
                    write_frame(
                        &mut writer.lock().await.input,
                        &Request::SensitiveAnswer { prompt_id, answer },
                    )
                    .await?;
                    Ok(Some(prompt_id))
                });
                prompt_tasks.insert(prompt_id, cancellation);
            }
            Response::Tool { request_id } => results.terminal(request_id)?,
            Response::Authorization {
                request_id,
                authorization_id,
                tool,
                mut permissions,
                arguments,
            } => {
                rebase_remote_permissions(&target, &mut permissions)?;
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .filter(|pending| !pending.sender.is_closed())
                    .map(|pending| pending.context.clone());
                let writer = host.0.clone();
                let coordinator = host.1.clone();
                callbacks.spawn(async move {
                    let decision = if let Some(context) = context {
                        match context.invocation_subject() {
                            Ok(subject) => {
                                coordinator
                                    .authorize(subject, tool, permissions, arguments)
                                    .await
                            }
                            Err(error) => Err(AuthorizationError::Denied(error.to_string())),
                        }
                    } else {
                        Err(AuthorizationError::Denied(
                            "remote authorization requires a host tool context".into(),
                        ))
                    };
                    let (allowed, reason) = match decision {
                        Ok(()) => (true, None),
                        Err(
                            AuthorizationError::Denied(reason)
                            | AuthorizationError::InvalidGrant(reason),
                        ) => (false, Some(reason)),
                        Err(AuthorizationError::Cancelled) => {
                            (false, Some("tool was cancelled".into()))
                        }
                        Err(AuthorizationError::Unavailable) => {
                            (false, Some("capability is unavailable".into()))
                        }
                    };
                    write_frame(
                        &mut writer.lock().await.input,
                        &Request::AuthorizationDecision {
                            request_id,
                            authorization_id,
                            allowed,
                            reason,
                        },
                    )
                    .await?;
                    Ok(None)
                });
            }
            Response::Ready => {
                return Err(RemoteError::Protocol(
                    "received a second remote ready response".to_owned(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        fixture_context, output, route_fixture, test_connection, write_result,
    };
    use super::*;
    use crate::remote::protocol::AuthorizationId;
    use crate::tool::policy::{Capability, PermissionUse, ResourceId};
    use std::time::Duration;

    const PROMPT: PromptId = PromptId(9);
    const AUTHORIZATION: AuthorizationId = AuthorizationId(7);

    async fn control(reader: &mut tokio::io::DuplexStream) -> Option<Request> {
        loop {
            let request = read_frame(reader).await.unwrap();
            if !matches!(request, Some(Request::PayloadAck)) {
                return request;
            }
        }
    }

    async fn register(
        state: &Mutex<ConnectionState>,
        id: u64,
        context: &ToolContext,
    ) -> oneshot::Receiver<super::super::PendingResult> {
        let (sender, receiver) = oneshot::channel();
        let call = PendingCall {
            sender,
            context: context.clone(),
        };
        state
            .lock()
            .await
            .pending
            .insert(RequestId::new(id).unwrap(), call);
        receiver
    }

    #[tokio::test]
    async fn authorization_callbacks_do_not_block_dispatch_and_fail_the_connection_on_write_error()
    {
        use crate::tool::policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture};
        use tokio::sync::Notify;
        struct WaitingPolicy(Arc<Notify>, Arc<Notify>);
        impl Policy for WaitingPolicy {
            fn authorize(&self, _: AuthorizationRequest) -> PolicyFuture<'_> {
                Box::pin(async move {
                    self.0.notify_one();
                    self.1.notified().await;
                    PolicyDecision::allow()
                })
            }
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let connection = test_connection().await;
        let (mut requests, responses) = tokio::io::duplex(4096);
        let (input, mut replies) = tokio::io::duplex(4096);
        let writer = Arc::new(Mutex::new(RequestWriter {
            input: Box::new(input),
            next_request_id: Some(RequestId::FIRST),
        }));
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let policy = WaitingPolicy(entered.clone(), release.clone());
        let authorization = AuthorizationCoordinator::new(Arc::new(policy));
        let first_result = register(&connection.state, 1, &context).await;
        let receiver = register(&connection.state, 2, &context).await;
        let state = connection.state.clone();
        let reader = tokio::spawn(async move {
            let prompts = Arc::new(crate::remote::RejectSensitivePrompts);
            let callbacks = (&writer, &authorization);
            route_responses(responses, &state, callbacks, "build".into(), prompts).await;
        });
        let prompt = Response::SensitivePrompt {
            prompt_id: PROMPT,
            prompt: crate::remote::SensitivePrompt {
                kind: crate::remote::SensitivePromptKind::Password,
                message: "fixture".into(),
            },
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            let outside = ResourceId::path("root", std::path::Path::new("/outside"));
            let request = Response::Authorization {
                request_id: RequestId::FIRST,
                authorization_id: AUTHORIZATION,
                tool: "read".into(),
                permissions: vec![PermissionUse::new(Capability::Read, outside)],
                arguments: serde_json::json!({}),
            };
            write_frame(&mut requests, &request).await.unwrap();
            entered.notified().await;
            write_result(&mut requests, RequestId::new(2).unwrap(), output("second")).await;
            let completed = receiver.await.unwrap().unwrap();
            assert_eq!(completed.0.unwrap().value, "second");
            write_frame(&mut requests, &prompt).await.unwrap();
            assert!(matches!(
                control(&mut replies).await,
                Some(Request::SensitiveAnswer {
                    prompt_id: PROMPT,
                    answer: crate::remote::prompt::PromptAnswer::Rejected
                })
            ));
            release.notify_one();
            assert!(matches!(
                control(&mut replies).await,
                Some(Request::AuthorizationDecision {
                    authorization_id: AUTHORIZATION,
                    allowed: true,
                    ..
                })
            ));
        })
        .await
        .unwrap();
        // No further incoming frame should be required to observe callback failure.
        drop(replies);
        write_frame(&mut requests, &prompt).await.unwrap();
        let failure = tokio::time::timeout(Duration::from_secs(2), first_result).await;
        assert!(matches!(
            failure.unwrap().unwrap(),
            Err(RemoteError::Io { .. })
        ));
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_or_closed_response_fails_pending_requests() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        for orphan in [true, false] {
            let (mut peer, stream) = tokio::io::duplex(4096);
            let state = Arc::new(Mutex::new(ConnectionState::default()));
            let receiver = register(&state, 1, &context).await;
            let reader = tokio::spawn(async move { route_fixture(stream, &state, "test").await });
            if orphan {
                let orphaned = Response::Tool {
                    request_id: RequestId::new(99).unwrap(),
                };
                write_frame(&mut peer, &orphaned).await.unwrap();
            }
            drop(peer);
            assert!(matches!(
                receiver.await.unwrap(),
                Err(RemoteError::Protocol(_))
            ));
            reader.await.unwrap();
        }
    }
}
