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
        "ordinary remote commands receive the forwarded central agent"
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
