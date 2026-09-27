//! Dispatch shim responses without blocking frame reads on host callbacks.
use super::permissions::rebase_remote_permissions;
use super::*;
use crate::{
    remote::{PromptAnswer, flow::CHUNK_BYTES, protocol::ControlRequest},
    target::TargetRef,
    tool::authorization::AuthorizationError,
};
use tokio::io::AsyncRead;

pub(super) async fn route_responses<R>(
    output: R,
    state: &Mutex<ConnectionState>,
    writer: &Arc<Mutex<Writer>>,
    location: ExecutionLocation,
    prompts: Arc<dyn SensitivePromptHandler>,
    shutdown: CancellationToken,
    owner: Box<dyn Send>,
) where
    R: AsyncRead + Unpin,
{
    let mut results = super::results::Results::default();
    let error = tokio::select! {
        Err(error) = route(output, state, writer, location, prompts, &mut results) => error,
        () = shutdown.cancelled() => RemoteError::Cancelled,
    };
    // Stop transport resources before draining: persistence must not need a live
    // socket or acknowledgements, and eviction must not abort accepted payloads.
    drop(owner);
    let error = transport_error(error, Operation::Receive);
    state.lock().await.fail(error.clone());
    results.shutdown(state).await;
    fail_connection(state, error).await;
}

async fn route<R>(
    mut output: R,
    state: &Mutex<ConnectionState>,
    writer: &Arc<Mutex<Writer>>,
    location: ExecutionLocation,
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
                            Some(Err(error)) => return Err(RemoteError::task_failed("remote callback", error)),
                            None => unreachable!("nonempty callback set"),
                        }
                    }
                }
            }
        };
        let response = response
            .map_err(|error| transport_error(error, Operation::Receive))?
            .ok_or(RemoteError::Protocol(ProtocolError::Violation(
                "shim closed before replying",
            )))?;
        match response {
            Response::Payload { request_id, event } => {
                results
                    .payload(state, writer, &location, request_id, event)
                    .await?;
            }
            Response::SensitiveCancelled { prompt_id } => {
                if let Some(cancellation) = prompt_tasks.remove(&prompt_id) {
                    cancellation.cancel();
                }
            }
            Response::StreamData { channel, data } => {
                let invalid = data.len() > CHUNK_BYTES;
                let overflow = {
                    let state = state.lock().await;
                    state.streams.get(&channel).is_some_and(|sender| {
                        !sender.output.is_closed() && sender.output.try_send(data).is_err()
                    })
                };
                if invalid || overflow {
                    return Err(ProtocolError::Violation(
                        "invalid stream output or flow-control overflow",
                    )
                    .into());
                }
            }
            Response::StreamClosed { channel, error } => {
                if let Some(stream) = state.lock().await.streams.remove(&channel) {
                    let result = error.map_or(Ok(()), |mut diagnostic| {
                        diagnostic.bind_worker(&location);
                        Err(RemoteError::Remote {
                            diagnostic: Box::new(diagnostic.into()),
                            output: None,
                        })
                    });
                    let _ = stream.closed.send(result);
                }
            }
            Response::StreamAck { channel } => {
                let invalid = {
                    let state = state.lock().await;
                    state
                        .streams
                        .get(&channel)
                        .is_some_and(|stream| stream.credit.0.acknowledge().is_err())
                };
                if invalid {
                    return Err(ProtocolError::Violation("invalid stream credit").into());
                }
            }
            Response::SourceAck { request_id } => {
                // Acknowledgements may trail a call that has already ended.
                let invalid = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .is_some_and(|pending| {
                        pending
                            .upload
                            .as_ref()
                            .is_none_or(|credits| credits.0.acknowledge().is_err())
                    });
                if invalid {
                    return Err(ProtocolError::Violation("invalid source upload credit").into());
                }
            }
            Response::SensitivePrompt {
                prompt_id,
                mut prompt,
            } => {
                if prompt_tasks.contains_key(&prompt_id) {
                    return Err(ProtocolError::Violation("duplicate prompt").into());
                }
                prompt.origin = location.target.clone();
                let writer = writer.clone();
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
                    let answer = answer.unwrap_or(PromptAnswer::Rejected);
                    let answer = ControlRequest::SensitiveAnswer { prompt_id, answer };
                    write_frame(&mut *writer.lock().await, &Request::Control(answer))
                        .await
                        .map_err(|error| transport_error(error, Operation::Send))?;
                    Ok(Some(prompt_id))
                });
                prompt_tasks.insert(prompt_id, cancellation);
            }
            Response::Tool { request_id } => results.terminal(request_id)?,
            Response::Authorization {
                request_id,
                authorization_id,
                request,
            } => {
                // The worker names its own machine as the root; everything it asks
                // for is bound to this connection's target.
                let (mut permissions, origin) = request.into_parts(&TargetRef::Root);
                rebase_remote_permissions(&location.target, &mut permissions)?;
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .filter(|pending| !pending.sender.is_closed())
                    .map(|pending| pending.context.clone());
                let writer = writer.clone();
                callbacks.spawn(async move {
                    let decision = match context {
                        Some(context) => context.authorize(permissions, origin).await,
                        None => Err(AuthorizationError::PolicyFailed),
                    };
                    write_frame(
                        &mut *writer.lock().await,
                        &Request::AuthorizationDecision {
                            request_id,
                            authorization_id,
                            decision: decision.into(),
                        },
                    )
                    .await
                    .map_err(|error| transport_error(error, Operation::Send))?;
                    Ok(None)
                });
            }
            Response::Ready => {
                return Err(
                    ProtocolError::Violation("received a second remote ready response").into(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        build, fixture_context, fixture_context_with_capabilities, output, route_fixture,
        test_connection, write_result,
    };
    use super::*;
    use crate::remote::protocol::{AuthorizationDecision, AuthorizationId};
    use crate::tests::bounded;
    use crate::tool::authorization::{
        AuthorizationArguments, AuthorizationCoordinator, Reauthorization,
    };
    use crate::tool::policy::{Capability, PermissionUse, ResourceId};

    const PROMPT: PromptId = PromptId::new(9);
    const AUTHORIZATION: AuthorizationId = AuthorizationId::new(7);

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
            upload: None,
        };
        state.lock().await.pending.insert(RequestId::new(id), call);
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
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let policy = WaitingPolicy(entered.clone(), release.clone());
        let context = fixture_context(&runtime).with_invocation_authority(
            AuthorizationCoordinator::new(Arc::new(policy)),
            "read".into(),
            AuthorizationArguments::default(),
        );
        let connection = test_connection();
        let (mut requests, responses) = tokio::io::duplex(4096);
        let (input, mut replies) = tokio::io::duplex(4096);
        let writer: Arc<Mutex<Writer>> = Arc::new(Mutex::new(Box::new(input)));
        let first_result = register(&connection.state, 1, &context).await;
        let receiver = register(&connection.state, 2, &context).await;
        let state = connection.state.clone();
        let reader = tokio::spawn(async move {
            let prompts = Arc::new(crate::remote::RejectSensitivePrompts);
            route_responses(
                responses,
                &state,
                &writer,
                build(),
                prompts,
                CancellationToken::new(),
                Box::new(()),
            )
            .await;
        });
        let prompt = || Response::SensitivePrompt {
            prompt_id: PROMPT,
            prompt: crate::remote::SensitivePrompt::test(
                crate::remote::SensitivePromptKind::Password,
            ),
        };
        bounded(async {
            let outside = ResourceId::path(
                &crate::target::TargetRef::Root,
                &crate::tool::policy::PathText::new("/outside").unwrap(),
            );
            let request = Response::Authorization {
                request_id: RequestId::new(1),
                authorization_id: AUTHORIZATION,
                request: Reauthorization::Permissions(vec![PermissionUse::new(
                    Capability::Read,
                    outside,
                )]),
            };
            write_frame(&mut requests, &request).await.unwrap();
            entered.notified().await;
            write_result(&mut requests, RequestId::new(2), output("second")).await;
            let completed = receiver.await.unwrap().unwrap();
            assert_eq!(completed.output.value, "second");
            write_frame(&mut requests, &prompt()).await.unwrap();
            assert!(matches!(
                control(&mut replies).await,
                Some(Request::Control(ControlRequest::SensitiveAnswer {
                    prompt_id: PROMPT,
                    answer: PromptAnswer::Rejected
                }))
            ));
            release.notify_one();
            assert!(matches!(
                control(&mut replies).await,
                Some(Request::AuthorizationDecision {
                    authorization_id: AUTHORIZATION,
                    decision: AuthorizationDecision::Allowed,
                    ..
                })
            ));
        })
        .await;
        // No further incoming frame should be required to observe callback failure.
        drop(replies);
        write_frame(&mut requests, &prompt()).await.unwrap();
        let error = bounded(first_result)
            .await
            .unwrap()
            .unwrap_err()
            .into_tool_error()
            .diagnostic();
        assert_eq!(error.context.operation, Operation::Send);
        assert_eq!(error.context.site, FailureSite::Host);
        assert_eq!(error.context.effects, Effects::MayHaveExecuted);
        assert!(matches!(
            error.cause,
            crate::tool::diagnostic::Cause::Io { .. }
        ));
        reader.await.unwrap();
    }

    /// A worker's network request is bound to the connection it arrives on, not
    /// to the target of the call it serves, such as a source read for a tool
    /// running elsewhere.
    #[tokio::test]
    async fn network_reauthorization_is_bound_to_the_connection_target() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = crate::tests::RecordingPolicy::allowing();
        let capabilities = [Capability::Network].into_iter().collect();
        let context = fixture_context_with_capabilities(&runtime, capabilities)
            .with_invocation_authority(
                AuthorizationCoordinator::new(policy.clone()),
                "read".into(),
                AuthorizationArguments::default(),
            );
        assert_eq!(context.execution_location().target, TargetRef::Root);
        let connection = test_connection();
        let (mut requests, responses) = tokio::io::duplex(4096);
        let (input, mut replies) = tokio::io::duplex(4096);
        let writer: Arc<Mutex<Writer>> = Arc::new(Mutex::new(Box::new(input)));
        let _pending = register(&connection.state, 1, &context).await;
        let state = connection.state.clone();
        let reader = tokio::spawn(async move {
            let prompts = Arc::new(crate::remote::RejectSensitivePrompts);
            route_responses(
                responses,
                &state,
                &writer,
                build(),
                prompts,
                CancellationToken::new(),
                Box::new(()),
            )
            .await;
        });
        let origin = "https://example.test";
        let request = Response::Authorization {
            request_id: RequestId::new(1),
            authorization_id: AUTHORIZATION,
            request: Reauthorization::Network(origin.to_owned()),
        };
        write_frame(&mut requests, &request).await.unwrap();
        assert!(matches!(
            bounded(control(&mut replies)).await,
            Some(Request::AuthorizationDecision {
                decision: AuthorizationDecision::Allowed,
                ..
            })
        ));
        {
            let seen = policy.requests.lock().unwrap();
            let [request] = &seen[..] else {
                panic!("one request: {seen:?}");
            };
            let network = ResourceId::network(&build().target, origin);
            let expected = [PermissionUse::new(Capability::Network, network)];
            assert_eq!(request.permissions, expected);
            assert_eq!(request.arguments["network_origin"], origin);
        }
        drop(requests);
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_or_closed_response_fails_pending_requests() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        for (orphan, invalid_frame) in [(true, false), (false, false), (false, true)] {
            let (mut peer, stream) = tokio::io::duplex(4096);
            let state = Arc::new(Mutex::new(ConnectionState::default()));
            let receiver = register(&state, 1, &context).await;
            let reader = tokio::spawn(async move { route_fixture(stream, &state).await });
            if orphan {
                let orphaned = Response::Tool {
                    request_id: RequestId::new(99),
                };
                write_frame(&mut peer, &orphaned).await.unwrap();
            }
            if invalid_frame {
                use tokio::io::AsyncWriteExt as _;
                peer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
            }
            drop(peer);
            let error = receiver
                .await
                .unwrap()
                .unwrap_err()
                .into_tool_error()
                .diagnostic();
            assert_eq!(error.context.operation, Operation::Receive);
            assert_eq!(error.context.site, FailureSite::Host);
            assert_eq!(
                error.context.subject,
                Subject::Label("remote transport".into())
            );
            assert_eq!(error.context.effects, Effects::MayHaveExecuted);
            if invalid_frame {
                assert!(matches!(
                    error.cause,
                    crate::tool::diagnostic::Cause::Io { .. }
                ));
            }
            reader.await.unwrap();
        }
    }
}
