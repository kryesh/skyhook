use super::*;
use crate::{
    remote::prompt::RejectSensitivePrompts,
    target::{SshOptions, TargetConfig, TargetConfigType, TargetSource},
};

fn resolved() -> ResolvedSsh {
    parse_resolved_ssh(
        "hostname example.test\nuser remote-user\nport 22\n\
         identityfile ~/.ssh/id_ed25519\nidentitiesonly yes\n\
         sendenv *\nSendEnv SKYHOOK_TEST_DOTENV_KEY\n\
         setenv SKYHOOK_TEST_CONFIG_KEY=local-config-secret\n\
         SeTeNv SKYHOOK_TEST_DOTENV_KEY=local-dotenv-secret\n\
         forwardx11 yes\nForwardX11Trusted yes\n",
    )
    .unwrap()
}

fn target(name: &str) -> TargetDefinition {
    let mut target = TargetDefinition::from_config(
        name.into(),
        TargetConfig {
            r#type: TargetConfigType::Ssh,
            host: "example.test".into(),
            workspace: "/remote/work".into(),
            via: None,
            ssh: SshOptions::default(),
        },
        TargetSource::Config,
    )
    .unwrap();
    target.resolved = Some(resolved());
    target
}

#[test]
fn resolved_options_never_put_environment_forwarding_on_the_wire() {
    let target = target("destination");
    let resolved = target.resolved.as_ref().unwrap();
    assert_eq!(resolved.identity_files, ["~/.ssh/id_ed25519"]);
    assert_eq!(resolved.options["identitiesonly"], ["yes"]);
    assert!(
        resolved
            .options
            .keys()
            .all(|key| !forwards_environment(key))
    );

    // Resolved targets travel in both ResolveSsh responses and OpenSsh routes.
    // Local SetEnv literals must not appear in either serialized representation.
    for wire in [
        serde_json::to_string(resolved).unwrap(),
        serde_json::to_string(&crate::remote::protocol::Request::OpenSsh {
            channel: 1,
            route: vec![target],
            command: "exec remote-worker --serve /remote/work".into(),
        })
        .unwrap(),
    ] {
        assert!(!wire.contains("SKYHOOK_TEST_"), "{wire}");
        assert!(!wire.contains("local-config-secret"), "{wire}");
        assert!(!wire.contains("local-dotenv-secret"), "{wire}");
    }
}

#[tokio::test]
async fn generated_config_filters_pre_resolved_forwarding_for_every_hop() {
    let mut route = vec![target("jump"), target("destination")];
    for target in &mut route {
        // Defense in depth for externally supplied/pre-resolved options. SSH
        // option names are case-insensitive even if ssh -G normally lowers them.
        let options = &mut target.resolved.as_mut().unwrap().options;
        for key in ["SendEnv", "sendenv", "SENDENV"] {
            options.insert(key.into(), vec!["* SKYHOOK_TEST_DOTENV_KEY".into()]);
        }
        for key in ["SetEnv", "setenv", "SETENV"] {
            options.insert(key.into(), vec!["SKYHOOK_TEST_CONFIG_KEY=secret".into()]);
        }
        options.insert("ForwardX11".into(), vec!["yes".into()]);
        options.insert("ForwardX11Trusted".into(), vec!["yes".into()]);
    }
    let config = SshConfig::create(&route, Arc::new(RejectSensitivePrompts))
        .await
        .unwrap();
    let text = std::fs::read_to_string(&config.path).unwrap();
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("sendenv"), "{text}");
    assert!(!lower.contains("setenv"), "{text}");
    assert!(!lower.contains("secret"), "{text}");
    assert!(!lower.contains("skyhook_test_"), "{text}");
    assert!(!lower.contains("include"), "{text}");
    assert!(!lower.contains("forwardx11 yes"), "{text}");
    assert_eq!(lower.matches("forwardx11 no").count(), route.len());
    assert_eq!(lower.matches("forwardx11trusted no").count(), route.len());
    assert!(text.contains("ProxyJump skyhook-target-0"), "{text}");
    assert!(
        text.contains("IdentityFile \"~/.ssh/id_ed25519\""),
        "{text}"
    );
    assert!(text.contains("IdentityAgent SSH_AUTH_SOCK"), "{text}");

    let command = ssh_command(&config, &config.destination);
    let args: Vec<_> = command.as_std().get_args().collect();
    assert_eq!(args[0], "-F");
    assert_eq!(args[1], config.path.as_os_str());
    // Agent forwarding is a deliberate internal protocol, not SendEnv.
    assert!(args.contains(&std::ffi::OsStr::new("-A")));
    assert!(args.contains(&std::ffi::OsStr::new("-T")));
    // No local environment values or secrets belong in remote command arguments.
    assert!(
        args.iter()
            .all(|arg| !arg.to_string_lossy().contains("secret"))
    );
}
