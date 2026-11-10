//! A route's last leg as the machine that starts it, root or an origin's shim,
//! receives it.
use super::error::RemoteError;
use crate::target::{
    ConfiguredPasswords, Connection, Password, Route, SshAuth, SshOption, SshOptions,
    TargetDefinition, WinrmOptions,
};
use skyhook_tools::target::TargetName;
use skyhook_worker::origin::{
    Leg,
    error::SshError,
    ssh::{Hop, HostKeyChecking, HostKeyUpdates, SshRoute, SshValue},
    winrm::{Endpoint, WinrmRoute},
};
use skyhook_worker::secret::Credential;

/// The seconds a hop's connect is given unless its target's options say, as long as
/// a WinRM connect is.
const CONNECT_TIMEOUT: &str = "30";

/// `route`'s jumps and destination, in the transport the destination takes, each
/// password the credential `passwords` holds for it.
pub(super) fn leg(route: &Route, passwords: &ConfiguredPasswords) -> Result<Leg, RemoteError> {
    let destination = &route.destination;
    let credential = |target: &TargetName, password: &Password| {
        Ok(passwords.credential(target, password)?.clone())
    };
    Ok(match &destination.connection {
        Connection::Ssh(ssh) => Leg::Ssh(ssh_route(route, ssh, credential)?),
        Connection::Winrm(winrm) => {
            let password = credential(&destination.name, winrm.auth.password())?;
            Leg::Winrm(winrm_route(destination, winrm, password))
        }
    })
}

/// The hops of `route` as the SSH process its origin starts receives them, the one
/// target whose secret that process types placed by construction: the registry
/// admits a route with at most one.
fn ssh_route(
    route: &Route,
    destination: &SshOptions,
    credential: impl Fn(&TargetName, &Password) -> Result<Credential, RemoteError>,
) -> Result<SshRoute, RemoteError> {
    let jumps = (route.jumps.iter()).map(|jump| (&jump.name, &*jump.host, &jump.ssh));
    let target = &route.destination;
    let hops = jumps.chain([(&target.name, &*target.host, destination)]);
    let (mut before, mut secret, mut after) = (Vec::new(), None, Vec::new());
    for (name, host, ssh) in hops {
        let hop = hop(name, host, ssh)?;
        match (&secret, ssh.auth.password()) {
            (Some(_), _) => after.push(hop),
            (None, Some(password)) => secret = Some((hop, credential(name, password)?)),
            (None, None) => before.push(hop),
        }
    }
    Ok(match secret {
        None => SshRoute::Plain(before),
        Some((hop, credential)) => SshRoute::Secret {
            before,
            hop,
            credential,
            after,
        },
    })
}

fn hop(name: &TargetName, host: &str, ssh: &SshOptions) -> Result<Hop, SshError> {
    let value = |text: &str| SshValue::try_from(text.to_owned());
    let key = match &ssh.auth {
        SshAuth::Key { path, .. } => Some(value(path.as_str())?),
        SshAuth::Agent | SshAuth::Password { .. } => None,
    };
    let (mut host_keys, mut host_key_updates, mut options) = (None, None, Vec::new());
    for (key, option) in &ssh.options {
        match SshOption::parse(key) {
            SshOption::StrictHostKeyChecking => {
                let checking =
                    HostKeyChecking::of(option.as_str()).ok_or(SshError::InvalidValue)?;
                // OpenSSH takes an option's first value.
                host_keys.get_or_insert(checking);
            }
            SshOption::UpdateHostKeys => {
                let updates = HostKeyUpdates::of(option.as_str()).ok_or(SshError::InvalidValue)?;
                host_key_updates.get_or_insert(updates);
            }
            _ => options.push((key.clone(), option.clone())),
        }
    }
    // Nothing else ends a connect that stalls; OpenSSH's own error then fails it.
    let timed = |key| matches!(SshOption::parse(key), SshOption::ConnectTimeout);
    if !ssh.options.keys().any(timed) {
        options.push(("ConnectTimeout".parse()?, value(CONNECT_TIMEOUT)?));
    }
    Ok(Hop {
        target: name.to_string(),
        host: value(host)?,
        port: ssh.port,
        user: ssh.user.clone(),
        key,
        forward_agent: ssh.forward_agent,
        host_keys: host_keys.unwrap_or_default(),
        host_key_updates,
        options,
    })
}

/// The WinRM listener `target` names, as the machine connecting to it receives it.
fn winrm_route(
    target: &TargetDefinition,
    winrm: &WinrmOptions,
    password: Credential,
) -> WinrmRoute {
    WinrmRoute {
        target: target.name.to_string(),
        endpoint: Endpoint {
            host: target.host.clone(),
            port: winrm.port,
        },
        auth: winrm.auth.clone().with_password(password),
        tls: winrm.tls.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{ConfiguredTargets, TargetRegistry};
    use skyhook_tools::error::AdmissionError;
    use skyhook_worker::diagnostic::Operation;

    /// The hop whose definition holds the route's one secret types it; the hops
    /// before and after it keep their order. A session's configured target types
    /// the password the configuration gives it, and connects only while the
    /// configuration defines the target as the session holds it.
    #[test]
    fn the_secret_hop_is_placed_by_construction() {
        let definition = crate::target::tests::definition;
        let mut db = definition("db", "/work", Some("jump"));
        let password = Credential::Literal("pw".into());
        db.ssh_mut().auth = SshAuth::Password {
            password: Password::Given(password.clone()),
        };
        let others = [
            definition("jump", "/work", None),
            definition("inner", "/work", Some("db")),
        ];
        let configured = |db: &TargetDefinition| {
            ConfiguredTargets::of(others.iter().chain([db]).cloned().collect())
        };
        let targets = configured(&db).definitions();
        let session = TargetRegistry::from_definitions(targets.clone()).unwrap();
        let passwords = configured(&db).passwords(&session);
        let routed = |targets: Vec<TargetDefinition>, passwords: &ConfiguredPasswords| {
            let registry = TargetRegistry::from_definitions(targets).unwrap();
            leg(
                &registry.route(&"inner".parse().unwrap()).unwrap(),
                passwords,
            )
        };
        let leg = |targets: Vec<TargetDefinition>| {
            let Ok(Leg::Ssh(route)) = routed(targets, &passwords) else {
                panic!("an SSH destination takes an SSH leg");
            };
            route
        };
        let names = |hops: &[Hop]| hops.iter().map(|hop| hop.target.clone()).collect();
        let SshRoute::Secret {
            before,
            hop,
            credential,
            after,
        } = leg(targets.to_vec())
        else {
            panic!("a password hop types its secret");
        };
        let placed: (Vec<String>, _, Vec<String>) =
            (names(&before), hop.target.as_str(), names(&after));
        assert_eq!(placed, (vec!["jump".into()], "db", vec!["inner".into()]));
        assert_eq!(credential, password);
        // The configuration's password for another host is not this session's.
        db.host = "moved.example.com".into();
        let moved = configured(&db).passwords(&session);
        let unconfigured = routed(targets.clone(), &moved).unwrap_err();
        let fault = &unconfigured.fault;
        assert!(matches!(
            fault,
            crate::remote::RemoteFault::PasswordUnconfigured(_)
        ));
        // The connection failed; no argument of the call was invalid.
        let failed = AdmissionError::from(unconfigured).diagnostic().context;
        assert_eq!(failed.operation, Operation::Connect);
        let mut plain = targets.to_vec();
        plain[2].ssh_mut().auth = SshAuth::Agent;
        assert!(matches!(leg(plain), SshRoute::Plain(hops) if hops.len() == 3));
    }

    /// A hop never asks whether to trust a host key: OpenSSH's default `ask` refuses
    /// an unknown key, and a configured `accept-new` or `no` is kept.
    #[test]
    fn host_key_checking_never_asks() {
        let checking = |value: Option<&str>| {
            let mut db = crate::target::tests::definition("db", "/work", None);
            let options = &mut db.ssh_mut().options;
            options.insert(
                "ServerAliveInterval".parse().unwrap(),
                "15".parse().unwrap(),
            );
            if let Some(value) = value {
                options.insert(
                    "StrictHostKeyChecking".parse().unwrap(),
                    value.parse().unwrap(),
                );
            }
            let ssh = db.ssh_mut().clone();
            let hop = hop(&db.name, &db.host, &ssh).unwrap();
            // The option beside it and the connect's time limit.
            assert_eq!(hop.options.len(), 2);
            hop.host_keys
        };
        for (value, expected) in [
            (None, HostKeyChecking::Yes),
            (Some("ASK"), HostKeyChecking::Yes),
            (Some("accept-new"), HostKeyChecking::AcceptNew),
            (Some("off"), HostKeyChecking::No),
        ] {
            assert_eq!(checking(value), expected, "{value:?}");
        }
    }

    /// A hop's connect is given a time limit, which its target's options replace.
    #[test]
    fn a_connect_is_timed_unless_its_target_times_it() {
        let mut db = crate::target::tests::definition("db", "/work", None);
        let option = |key: &str, value: &str| (key.parse().unwrap(), value.parse().unwrap());
        let options = |db: &TargetDefinition| {
            let ssh = db.connection.ssh().unwrap();
            hop(&db.name, &db.host, ssh).unwrap().options
        };
        assert_eq!(options(&db), [option("ConnectTimeout", "30")]);
        db.ssh_mut()
            .options
            .extend([option("connecttimeout", "none")]);
        assert_eq!(options(&db), [option("connecttimeout", "none")]);
    }

    /// A hop never asks whether to accept a host's rotated keys: `ask` declines them.
    #[test]
    fn host_key_updates_never_ask() {
        let mut db = crate::target::tests::definition("db", "/work", None);
        let options = &mut db.ssh_mut().options;
        options.insert("UpdateHostKeys".parse().unwrap(), "Ask".parse().unwrap());
        let ssh = db.ssh_mut().clone();
        let hop = hop(&db.name, &db.host, &ssh).unwrap();
        let asks =
            (hop.options.iter()).any(|(_, value)| value.as_str().eq_ignore_ascii_case("ask"));
        assert!(!asks, "{:?}", hop.options);
        assert_eq!(hop.host_key_updates, Some(HostKeyUpdates::No));
    }
}
