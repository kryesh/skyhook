//! Dispatch shim responses without blocking frame reads on host callbacks.
use super::permissions::rebase_remote_permissions;
use super::*;
use crate::tool::authorization::AuthorizationError;
use tokio::io::AsyncRead;

pub(super) async fn route_responses<R>(
    mut output: R,
    state: &Mutex<ConnectionState>,
    host: (&Arc<Mutex<RequestWriter>>, &AuthorizationCoordinator),
    target: String,
    prompts: Arc<dyn SensitivePromptHandler>,
) where
    R: AsyncRead + Unpin,
{
    let mut callbacks = tokio::task::JoinSet::<Result<Option<u64>, RemoteError>>::new();
    let mut prompt_tasks = HashMap::<u64, tokio::task::AbortHandle>::new();
    let mut results = super::results::Results::default();
    loop {
        // Keep the frame read alive while servicing callbacks: read_exact is not
        // cancellation-safe, so restarting it could lose part of a frame.
        let response = {
            let reading = read_frame::<_, Response>(&mut output);
            tokio::pin!(reading);
            loop {
                tokio::select! {
                    response = &mut reading => break response,
                    completed = callbacks.join_next(), if !callbacks.is_empty() => {
                        match completed {
                            Some(Ok(Ok(Some(id)))) => { prompt_tasks.remove(&id); }
                            Some(Ok(Ok(None))) => {}
                            Some(Err(error)) if error.is_cancelled() => {}
                            Some(Ok(Err(error))) => {
                                fail_connection(state, error).await;
                                return;
                            }
                            Some(Err(error)) => {
                                fail_connection(state, RemoteError::ConnectionTask(error.to_string())).await;
                                return;
                            }
                            None => unreachable!("nonempty callback set"),
                        }
                    }
                }
            }
        };
        let response = match response {
            Ok(Some(response)) => response,
            Ok(None) => {
                fail_connection(
                    state,
                    RemoteError::Protocol("shim closed before replying".to_owned()),
                )
                .await;
                return;
            }
            Err(error) => {
                fail_connection(state, RemoteError::io(error)).await;
                return;
            }
        };
        match response {
            Response::ToolArtifact {
                request_id,
                field,
                kind,
                offset,
                data,
                finished,
            } => {
                if let Err(error) = results
                    .artifact(
                        state,
                        super::results::ArtifactFrame {
                            request_id,
                            field,
                            kind,
                            offset,
                            data,
                            finished,
                        },
                    )
                    .await
                {
                    fail_connection(state, error).await;
                    return;
                }
            }
            Response::ToolChunk {
                request_id,
                offset,
                data,
                finished,
            } => {
                let result = match results.chunk(request_id, offset, data, finished) {
                    Ok(Some(result)) => result,
                    Ok(None) => continue,
                    Err(error) => {
                        fail_connection(state, RemoteError::io(error)).await;
                        return;
                    }
                };
                if let Err(error) = results.finish(state, request_id, result).await {
                    fail_connection(state, error).await;
                    return;
                }
            }
            Response::SensitiveCancelled { prompt_id } => {
                if let Some(task) = prompt_tasks.remove(&prompt_id) {
                    task.abort();
                }
            }
            Response::ResolvedSsh { request_id, result } => {
                if let Some(sender) = state.lock().await.resolutions.remove(&request_id) {
                    let _ = sender.send(result.map_err(RemoteError::Resolution));
                }
            }
            Response::StreamData { channel, data } => {
                let invalid = data.len() > 32 * 1024;
                let overflow = {
                    let state = state.lock().await;
                    state
                        .streams
                        .get(&channel)
                        .is_some_and(|sender| sender.output.try_send(Ok(data)).is_err())
                };
                if invalid || overflow {
                    fail_connection(
                        state,
                        RemoteError::Protocol(
                            "invalid stream output or flow-control overflow".into(),
                        ),
                    )
                    .await;
                    return;
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
                    state.streams.get(&channel).is_some_and(|stream| {
                        if stream.credit.available_permits() >= 16 {
                            true
                        } else {
                            stream.credit.add_permits(1);
                            false
                        }
                    })
                };
                if invalid {
                    fail_connection(state, RemoteError::Protocol("invalid stream credit".into()))
                        .await;
                    return;
                }
            }
            Response::SensitivePrompt {
                prompt_id,
                mut prompt,
            } => {
                prompt.message = format!("[origin={target}] {}", prompt.message);
                let writer = host.0.clone();
                let prompts = prompts.clone();
                let task = callbacks.spawn(async move {
                    let answer = match prompts.prompt(prompt).await {
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
                prompt_tasks.insert(prompt_id, task);
            }
            Response::Tool { request_id, result } => {
                if let Err(error) = results.finish(state, request_id, result).await {
                    fail_connection(state, error).await;
                    return;
                }
            }
            Response::Authorization {
                request_id,
                authorization_id,
                tool,
                mut permissions,
                arguments,
            } => {
                if let Err(error) = rebase_remote_permissions(&target, &mut permissions) {
                    fail_connection(state, error).await;
                    return;
                }
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .map(|pending| pending.context.clone());
                let writer = host.0.clone();
                let coordinator = host.1.clone();
                callbacks.spawn(async move {
                    let decision = if let Some(context) = context {
                        coordinator
                            .authorize(&context.authorization, tool, permissions, arguments)
                            .await
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
            Response::Ready { .. } => {
                fail_connection(
                    state,
                    RemoteError::Protocol("received a second remote ready response".to_owned()),
                )
                .await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_context, output, route_fixture, test_connection};
    use super::*;
    use crate::tool::policy::{Capability, PermissionUse, ResourceId};
    #[tokio::test]
    async fn authorization_callbacks_do_not_block_dispatch_and_fail_the_connection_on_write_error()
    {
        use crate::tool::policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture};
        struct WaitingPolicy {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        impl Policy for WaitingPolicy {
            fn authorize(&self, _: AuthorizationRequest) -> PolicyFuture<'_> {
                Box::pin(async move {
                    self.entered.notify_one();
                    self.release.notified().await;
                    PolicyDecision::allow()
                })
            }
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let connection = test_connection().await;
        let (mut requests, output) = tokio::io::duplex(4096);
        let (input, mut replies) = tokio::io::duplex(4096);
        let writer = Arc::new(Mutex::new(RequestWriter {
            input: Box::new(input),
            next_request_id: 1,
        }));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let authorization = AuthorizationCoordinator::new(Arc::new(WaitingPolicy {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let (sender, receiver) = oneshot::channel();
        let (first, first_result) = oneshot::channel();
        connection.state.lock().await.pending.insert(
            1,
            PendingCall {
                sender: first,
                context: context.clone(),
            },
        );
        connection
            .state
            .lock()
            .await
            .pending
            .insert(2, PendingCall { sender, context });
        let state = connection.state.clone();
        let reader = tokio::spawn(async move {
            route_responses(
                output,
                &state,
                (&writer, &authorization),
                "build".into(),
                Arc::new(crate::remote::RejectSensitivePrompts),
            )
            .await;
        });
        let prompt = Response::SensitivePrompt {
            prompt_id: 9,
            prompt: crate::remote::SensitivePrompt {
                kind: crate::remote::SensitivePromptKind::Password,
                message: "fixture".into(),
            },
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            write_frame(
                &mut requests,
                &Response::Authorization {
                    request_id: 1,
                    authorization_id: 7,
                    tool: "read".into(),
                    permissions: vec![PermissionUse::new(
                        Capability::Read,
                        ResourceId::new("path", ["root", "/", "outside"]),
                    )],
                    arguments: serde_json::json!({}),
                },
            )
            .await
            .unwrap();
            entered.notified().await;
            write_frame(
                &mut requests,
                &Response::Tool {
                    request_id: 2,
                    result: super::tests::output("second"),
                },
            )
            .await
            .unwrap();
            assert_eq!(receiver.await.unwrap().unwrap().unwrap().value, "second");
            write_frame(&mut requests, &prompt).await.unwrap();
            assert!(matches!(
                read_frame::<_, Request>(&mut replies).await.unwrap(),
                Some(Request::SensitiveAnswer {
                    prompt_id: 9,
                    answer: crate::remote::prompt::PromptAnswer::Rejected
                })
            ));
            release.notify_one();
            assert!(matches!(
                read_frame::<_, Request>(&mut replies).await.unwrap(),
                Some(Request::AuthorizationDecision {
                    authorization_id: 7,
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
        let failure = tokio::time::timeout(std::time::Duration::from_secs(2), first_result)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(failure, Err(RemoteError::Io { .. })));
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_or_closed_response_fails_pending_requests() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        for orphan in [true, false] {
            let (mut peer, stream) = tokio::io::duplex(4096);
            let state = Arc::new(Mutex::new(ConnectionState::default()));
            let (sender, receiver) = oneshot::channel();
            state.lock().await.pending.insert(
                1,
                PendingCall {
                    sender,
                    context: context.clone(),
                },
            );
            let reader = tokio::spawn(async move {
                route_fixture(stream, &state, "test").await;
            });
            if orphan {
                write_frame(
                    &mut peer,
                    &Response::Tool {
                        request_id: 99,
                        result: output("orphaned"),
                    },
                )
                .await
                .unwrap();
            }
            drop(peer);
            let expected = if orphan {
                "unknown request ID 99"
            } else {
                "closed before replying"
            };
            assert!(
                matches!(receiver.await.unwrap(), Err(RemoteError::Protocol(message)) if message.contains(expected))
            );
            reader.await.unwrap();
        }
    }
}
