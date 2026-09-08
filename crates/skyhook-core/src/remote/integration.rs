//! Disposable loopback SSH integration. Run with SKYHOOK_TEST_SHIM and --ignored.
use super::*;
use crate::{
    remote::{SecretValue, SensitivePrompt, SensitivePromptFuture},
    target::{SshOptions, TargetAuth, TargetConfig, TargetConfigType, TargetSource},
    tool::policy::AllowAll,
};
use std::{
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Prompts(AtomicUsize);
impl SensitivePromptHandler for Prompts {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(
                prompt.kind,
                crate::remote::SensitivePromptKind::KeyPassphrase,
                "unexpected prompt: {prompt:?}"
            );
            Ok(SecretValue::new("fixture-passphrase".into()))
        })
    }
}
struct Server {
    child: tokio::process::Child,
    directory: tempfile::TempDir,
    port: u16,
    user: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
impl Server {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        for (name, password) in [
            ("host", ""),
            ("first", ""),
            ("second", "fixture-passphrase"),
        ] {
            let result = tokio::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", password, "-f"])
                .arg(directory.path().join(name))
                .status()
                .await
                .unwrap();
            assert!(result.success());
        }
        let authorized = format!(
            "{}{}",
            std::fs::read_to_string(directory.path().join("first.pub")).unwrap(),
            std::fs::read_to_string(directory.path().join("second.pub")).unwrap()
        );
        std::fs::write(directory.path().join("authorized_keys"), authorized).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let user = String::from_utf8(
            tokio::process::Command::new("id")
                .arg("-un")
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let config = format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host\nAuthorizedKeysFile {0}/authorized_keys\nPidFile {0}/pid\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin yes\nAllowUsers {user}\nAllowTcpForwarding yes\nAllowAgentForwarding yes\nSetEnv HOME={0}\nLogLevel ERROR\n",
            directory.path().display()
        );
        std::fs::write(directory.path().join("sshd_config"), config).unwrap();
        let mut child = tokio::process::Command::new("/usr/bin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(directory.path().join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return Self {
                    child,
                    directory,
                    port,
                    user,
                };
            }
            assert!(child.try_wait().unwrap().is_none(), "fixture sshd failed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("fixture SSH server timed out")
    }
    async fn target(&self, name: &str, key: &str) -> TargetDefinition {
        let definition = TargetDefinition::from_config(
            name.into(),
            TargetConfig {
                r#type: TargetConfigType::Ssh,
                host: "127.0.0.1".into(),
                workspace: self.directory.path().into(),
                via: None,
                ssh: SshOptions {
                    user: Some(self.user.clone()),
                    port: Some(self.port),
                    auth: TargetAuth::Key {
                        path: self.directory.path().join(key),
                    },
                },
            },
            TargetSource::Config,
        )
        .unwrap();
        let mut definitions = crate::target::normalize::normalize(
            vec![definition],
            vec![],
            Arc::new(crate::target::normalize::LocalResolver),
        )
        .await
        .unwrap();
        let mut definition = definitions.remove(0);
        trust_fixture(&mut definition);
        definition
    }
}
fn trust_fixture(target: &mut TargetDefinition) {
    let options = &mut target.resolved.as_mut().unwrap().options;
    options.insert("stricthostkeychecking".into(), vec!["no".into()]);
    options.insert("userknownhostsfile".into(), vec!["/dev/null".into()]);
}
fn route(definitions: Vec<TargetDefinition>) -> ResolvedRoute {
    ResolvedRoute {
        identity: RouteIdentity {
            destination: definitions.last().unwrap().name.clone(),
            hops: definitions
                .iter()
                .map(|d| (d.name.clone(), d.revision))
                .collect(),
        },
        definitions,
    }
}

#[tokio::test]
#[ignore = "requires a built shim, sshd and loopback sockets; set SKYHOOK_TEST_SHIM"]
async fn native_jumps_and_shim_owned_connections_share_a_lazy_central_agent() {
    tokio::time::timeout(std::time::Duration::from_secs(120), exercise_connections())
        .await
        .expect("SSH integration timed out");
}
async fn exercise_connections() {
    let server = Server::start().await;
    let shim = std::fs::read(std::env::var_os("SKYHOOK_TEST_SHIM").expect("set SKYHOOK_TEST_SHIM"))
        .unwrap();
    let assets = Box::leak(
        vec![(
            "skyhook-shim-x86_64-linux",
            Box::leak(shim.into_boxed_slice()) as &'static [u8],
        )]
        .into_boxed_slice(),
    );
    let prompts = Arc::new(Prompts(AtomicUsize::new(0)));
    let manager = RemoteManager::new(
        EmbeddedShimCatalog::from_assets(assets).unwrap(),
        prompts.clone(),
        AuthorizationCoordinator::new(Arc::new(AllowAll)),
    );
    let first = server.target("first", "first").await;
    let mut native = first.clone();
    native.name = "native".into();
    native.via = Some("first".into());
    let cancel = CancellationToken::new();
    eprintln!("connecting first");
    let a = manager
        .connection(route(vec![first.clone()]), server.directory.path(), &cancel)
        .await
        .unwrap();
    eprintln!("connecting native jump");
    let b = manager
        .connection(
            route(vec![first.clone(), native]),
            server.directory.path(),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(prompts.0.load(Ordering::SeqCst), 0);
    let nested = server.target("nested", "second").await;
    let store = crate::session::SessionStore::create_ephemeral(server.directory.path())
        .await
        .unwrap();
    let router = crate::target::TargetRouter::new(
        crate::target::TargetRegistry::from_definitions([first.clone()]).unwrap(),
        manager.clone(),
        AuthorizationCoordinator::new(Arc::new(AllowAll)),
    );
    let mut capabilities = crate::tool::policy::CapabilitySet::default();
    capabilities.insert(crate::tool::policy::Capability::Targets);
    let subject = crate::tool::authorization::AuthorizationSubject {
        agent: crate::identity::AgentId::root(store.id()),
        job: crate::identity::JobId::new(1).unwrap(),
        parent: None,
        scope: None,
        capabilities,
        cancellation: cancel.clone(),
    };
    let mut registered = router
        .add(nested, "first".into(), &subject, &store)
        .await
        .unwrap();
    let mut nested = registered.remove(0);
    assert_eq!(nested.origin, "first");
    assert_eq!(nested.via.as_deref(), Some("first"));
    assert_eq!(nested.host, "127.0.0.1");
    assert_eq!(
        prompts.0.load(Ordering::SeqCst),
        0,
        "registration must not decrypt the destination key"
    );
    trust_fixture(&mut nested);
    eprintln!("connecting nested");
    let c = manager
        .connection(route(vec![first, nested]), server.directory.path(), &cancel)
        .await
        .unwrap();
    assert_eq!(
        prompts.0.load(Ordering::SeqCst),
        1,
        "encrypted remote key should be requested only when used"
    );
    let environment = manager.environment().await.unwrap();
    let identities = tokio::process::Command::new("ssh-add")
        .arg("-l")
        .envs(&environment)
        .output()
        .await
        .unwrap();
    assert!(identities.status.success());
    assert_eq!(
        String::from_utf8_lossy(&identities.stdout).lines().count(),
        2,
        "both keys belong to root's managed agent"
    );
    let (_, input) = tokio::sync::mpsc::channel(1);
    let context = ToolContext::new(
        subject,
        crate::execution::ExecutionLocation::named("nested", server.directory.path().into()),
        crate::execution::ExecutionLocation::root(server.directory.path().into()),
        input,
        crate::job::JobManager::new(store),
    );
    let listing = c
        .clone()
        .execute(
            "exec".into(),
            serde_json::json!({"argv":["ssh-add","-l"]}),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(listing.value["exit_code"], 0);
    assert_eq!(
        listing.value["stdout"].as_str().unwrap().lines().count(),
        2,
        "ordinary remote commands receive the central agent relay"
    );
    // Exercise duplex flow control well beyond a stream window.
    let mut transport = c
        .connection
        .clone()
        .open_ssh(vec![server.target("echo", "first").await], "cat".into())
        .await
        .unwrap();
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    let send = async {
        transport.input.write_all(&bytes).await.unwrap();
        transport.input.shutdown().await.unwrap();
    };
    let receive = async {
        let mut actual = Vec::new();
        transport.output.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, bytes);
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(send, receive);
    })
    .await
    .unwrap();
    drop(transport);
    drop(a);
    drop(b);
    drop(c);
    let socket = environment["SSH_AUTH_SOCK"].clone();
    manager.shutdown().await;
    assert!(!std::path::Path::new(&socket).exists());
}

#[tokio::test]
#[ignore = "requires a built shim, sshd and loopback sockets; set SKYHOOK_TEST_SHIM"]
async fn host_skills_from_remote_caller_copy_into_workspace_override() {
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        exercise_remote_skills(),
    )
    .await
    .expect("remote skills integration timed out");
}

async fn exercise_remote_skills() {
    use crate::{
        execution::ExecutionLocation,
        identity::AgentId,
        job::JobManager,
        session::SessionStore,
        target::{TargetRegistry, TargetRouter},
        tool::{
            ToolRegistryBuilder,
            builtins::{HostSkills, register_coding_tools},
            executor::ToolExecutor,
            policy::{
                AuthorizationRequest, Capability, Policy, PolicyDecision, PolicyFuture, ResourceId,
            },
        },
    };
    use serde_json::json;
    use std::path::Path;

    // Allow SSH setup and ordinary tool use, but require worker-side path authorization
    // to reject writes outside the remote caller's override, including symlink escapes.
    struct RemoteWorkspaceOnly {
        allowed: ResourceId,
        denied: AtomicUsize,
    }
    impl Policy for RemoteWorkspaceOnly {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            let outside = request.permissions.iter().any(|permission| {
                permission.capability == Capability::Write
                    && permission.resource.namespace == "path"
                    && permission.resource.segments.first().map(String::as_str) == Some("first")
                    && !permission
                        .resource
                        .segments
                        .starts_with(&self.allowed.segments)
            });
            if outside {
                self.denied.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async move {
                if outside {
                    PolicyDecision::Deny {
                        reason: "fixture denies writes outside remote override".into(),
                    }
                } else {
                    PolicyDecision::allow()
                }
            })
        }
    }

    fn copy_fixture(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let to = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_fixture(&entry.path(), &to);
            } else {
                std::fs::copy(entry.path(), to).unwrap();
            }
        }
    }

    let server = Server::start().await;
    let host = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let host_workspace = std::fs::canonicalize(host.path()).unwrap();
    let remote_override = std::fs::canonicalize(remote.path()).unwrap();
    let outside_workspace = std::fs::canonicalize(outside.path()).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/skill-workspace/.agents/skills/mixed-assets");
    let host_skill = host_workspace.join(".agents/skills/mixed-assets");
    copy_fixture(&fixture, &host_skill);

    let shim = std::fs::read(std::env::var_os("SKYHOOK_TEST_SHIM").expect("set SKYHOOK_TEST_SHIM"))
        .unwrap();
    let assets = Box::leak(
        vec![(
            "skyhook-shim-x86_64-linux",
            Box::leak(shim.into_boxed_slice()) as &'static [u8],
        )]
        .into_boxed_slice(),
    );
    let policy = Arc::new(RemoteWorkspaceOnly {
        allowed: ResourceId::path("first", &remote_override),
        denied: AtomicUsize::new(0),
    });
    let authorization = AuthorizationCoordinator::new(policy.clone());
    let manager = RemoteManager::new(
        EmbeddedShimCatalog::from_assets(assets).unwrap(),
        Arc::new(crate::remote::RejectSensitivePrompts),
        authorization.clone(),
    );
    let first = server.target("first", "first").await;
    let configured_workspace = first.workspace.clone();
    assert_ne!(configured_workspace, remote_override);
    assert_ne!(host_workspace, remote_override);
    let router = TargetRouter::new(
        TargetRegistry::from_definitions([first]).unwrap(),
        manager.clone(),
        authorization.clone(),
    );
    let store = SessionStore::create_ephemeral(&host_workspace)
        .await
        .unwrap();
    let jobs = JobManager::new(store.clone());
    let agent = AgentId::root(store.id());
    let mut builder = ToolRegistryBuilder::default();
    register_coding_tools(
        &mut builder,
        store.clone(),
        jobs.clone(),
        HostSkills::discover(&host_workspace).await,
        router.clone(),
    )
    .unwrap();
    let registry = builder.build();
    assert!(registry.get("__skill_copy").is_none());
    let surface = registry.surface(&Default::default());
    assert!(surface.get("__skill_copy").is_none());
    assert!(
        surface
            .definitions()
            .iter()
            .all(|tool| tool.name != "__skill_copy")
    );
    let script_surface = serde_json::to_value(surface.script_manifests()).unwrap();
    assert!(
        script_surface
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "__skill_copy")
    );
    assert!(
        surface.get("skill").unwrap().input_schema["properties"]
            .get("target")
            .is_none()
    );
    let mut capabilities = crate::tool::policy::CapabilitySet::default();
    capabilities.insert(Capability::Targets);
    let executor = ToolExecutor::with_authorization(
        registry,
        authorization,
        jobs.clone(),
        host_workspace.clone(),
    )
    .with_capabilities(capabilities)
    .with_location(ExecutionLocation::named("first", remote_override.clone()))
    .with_target_router(router);

    let listed = executor
        .execute_model(agent.clone(), "skills", json!({}), None)
        .await
        .unwrap();
    assert!(
        listed.output.value["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|skill| skill["name"] == "mixed-assets")
    );
    let loaded = executor
        .execute_model(
            agent.clone(),
            "skill",
            json!({"name":"mixed-assets", "path":null, "to":null}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(loaded.output.value["result"]["kind"], "skill");
    assert_eq!(
        loaded.output.value["result"]["content"],
        std::fs::read_to_string(host_skill.join("SKILL.md")).unwrap()
    );
    assert!(
        loaded.output.value["result"]["assets"]
            .as_str()
            .unwrap()
            .contains("payload.bin")
    );
    let text = executor
        .execute_model(
            agent.clone(),
            "skill",
            json!({"name":"mixed-assets", "path":"references/note.txt", "to":null}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(text.output.value["result"]["kind"], "text");
    assert_eq!(
        text.output.value["result"]["content"],
        std::fs::read_to_string(host_skill.join("references/note.txt")).unwrap()
    );
    let image = executor
        .execute_model(
            agent.clone(),
            "skill",
            json!({"name":"mixed-assets", "path":"assets/pixel.png"}),
            None,
        )
        .await
        .unwrap();
    let image_bytes = std::fs::read(host_skill.join("assets/pixel.png")).unwrap();
    assert_eq!(image.output.value["result"]["kind"], "image");
    assert_eq!(
        image.output.value["result"]["image"]["sha256"],
        crate::sha256_hex(&image_bytes)
    );
    assert_eq!(
        image.output.value["result"]["image"]["media_type"],
        "image/png"
    );
    assert_eq!(image.output.images.len(), 1);
    assert_eq!(
        serde_json::to_value(&image.output.images[0]).unwrap(),
        image.output.value["result"]["image"]
    );
    assert_eq!(
        store.read_blob(&image.output.images[0]).await.unwrap(),
        image_bytes
    );
    assert_eq!(jobs.images(image.job).await.unwrap(), image.output.images);

    // Loopback shares a filesystem, so use disjoint layouts: this relative parent
    // exists ONLY below the remote override, not below either other workspace.
    let relative = "remote-only/nested/copied.bin";
    std::fs::create_dir_all(remote_override.join("remote-only/nested")).unwrap();
    assert!(!host_workspace.join("remote-only").exists());
    assert!(!configured_workspace.join("remote-only").exists());
    assert!(!remote_override.join(".agents").exists());
    assert!(!configured_workspace.join(".agents").exists());
    let destination = remote_override.join(relative);
    std::fs::write(&destination, b"existing destination must be replaced").unwrap();
    // Both invalid UTF-8 and NUL-containing bytes must survive the worker transfer;
    // the second copy also proves that an already copied destination is overwritten.
    for asset in ["assets/payload.bin", "assets/nul.dat"] {
        let expected = std::fs::read(host_skill.join(asset)).unwrap();
        let copied = executor
            .execute_model(
                agent.clone(),
                "skill",
                json!({"name":"mixed-assets", "path":asset, "to":relative}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            copied.output.value["state"], "completed",
            "copy failed: {}",
            copied.output.value
        );
        assert_eq!(copied.output.value["result"]["kind"], "copied");
        assert_eq!(
            copied.output.value["result"]["to"],
            destination.to_string_lossy().as_ref()
        );
        assert_eq!(copied.output.value["result"]["bytes"], expected.len());
        assert_eq!(
            copied.output.value["result"]["sha256"],
            crate::sha256_hex(&expected)
        );
        let actual = std::fs::read(&destination).unwrap();
        assert_eq!(crate::sha256_hex(&actual), crate::sha256_hex(&expected));
        assert_eq!(actual, expected);
        assert_eq!(std::fs::read(host_skill.join(asset)).unwrap(), expected);
        assert!(!host_workspace.join("remote-only").exists());
        assert!(!configured_workspace.join("remote-only").exists());
    }

    let protected = outside_workspace.join("protected.bin");
    std::fs::write(&protected, b"outside sentinel").unwrap();
    std::os::unix::fs::symlink(&outside_workspace, remote_override.join("escape")).unwrap();
    for to in [
        protected.to_string_lossy().into_owned(),
        "escape/new.bin".into(),
    ] {
        let denied_before = policy.denied.load(Ordering::SeqCst);
        let result = executor
            .execute_model(
                agent.clone(),
                "skill",
                json!({"name":"mixed-assets", "path":"assets/payload.bin", "to":to}),
                None,
            )
            .await;
        let view = result.unwrap().output.value;
        assert_eq!(
            view["state"], "failed",
            "outside copy unexpectedly succeeded: {view}"
        );
        assert!(view["error"].as_str().is_some());
        assert!(
            policy.denied.load(Ordering::SeqCst) > denied_before,
            "copy must reach worker-side path authorization"
        );
        assert_eq!(std::fs::read(&protected).unwrap(), b"outside sentinel");
        assert!(!outside_workspace.join("new.bin").exists());
        assert_eq!(
            std::fs::read_dir(&outside_workspace).unwrap().count(),
            1,
            "denial must not leave temporary files"
        );
    }
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        std::fs::read(host_skill.join("assets/nul.dat")).unwrap()
    );
    manager.shutdown().await;
}
