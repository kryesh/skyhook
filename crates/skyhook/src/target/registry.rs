use std::collections::{BTreeMap, BTreeSet};

use crate::session::SessionEvent;
use crate::target::TargetError;
use crate::target::{
    Password, Revisions, Route, SshJump, TargetDefinition, TargetEdge, TargetSource,
};
use crate::tool::policy::ResourceId;
use skyhook_tools::target::{TargetName, TargetRef};

type Entries = BTreeMap<TargetName, TargetDefinition>;

/// The session's targets as its journal defines them: those it started with
/// and those it added since, each at its revision.
#[derive(Clone, Default)]
pub(crate) struct TargetRegistry {
    entries: Entries,
}

impl TargetRegistry {
    pub(crate) fn from_definitions(
        definitions: impl IntoIterator<Item = TargetDefinition>,
    ) -> Result<Self, TargetError> {
        let entries = (definitions.into_iter())
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        validate_graph(&entries)?;
        Ok(Self { entries })
    }

    /// Fold the definitions a journaled event records, as their revisions stand.
    pub(crate) fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::SessionStarted { targets, .. }
        | SessionEvent::TargetsUpserted { targets } = event
        {
            let named = targets
                .iter()
                .map(|target| (target.name.clone(), target.clone()));
            self.entries.extend(named);
        }
    }

    /// The named targets.
    pub(crate) fn list(&self) -> Vec<TargetDefinition> {
        self.entries.values().cloned().collect()
    }

    pub(crate) fn get(&self, name: &TargetName) -> Result<TargetDefinition, TargetError> {
        (self.entries.get(name).cloned()).ok_or_else(|| TargetError::Unknown(name.clone()))
    }

    pub(crate) fn route(&self, name: &TargetName) -> Result<Route, TargetError> {
        route(&self.entries, name, &mut BTreeSet::new())
    }

    /// `definition` as the session would add it, at its next revision.
    pub(crate) fn upsert(
        &self,
        mut definition: TargetDefinition,
    ) -> Result<TargetDefinition, TargetError> {
        let mut next = self.entries.clone();
        definition.source = TargetSource::Session;
        definition.revision = next
            .get(&definition.name)
            .map_or(1, |old| old.revision.saturating_add(1));
        next.insert(definition.name.clone(), definition.clone());
        validate_graph(&next)?;
        Ok(definition)
    }

    /// The revision of every target `resource` depends on, as those targets now
    /// stand: a named target and those it is routed through. None for a route
    /// that names another revision, or a target the session does not have.
    pub(crate) fn revisions(&self, resource: &ResourceId) -> Option<Revisions> {
        let route = |name: &TargetName| self.route(name).ok().map(|route| route.identity());
        match resource {
            ResourceId::Workspace { target, .. }
            | ResourceId::Path { target, .. }
            | ResourceId::Network { target, .. } => target.name().map_or(Some(Vec::new()), route),
            ResourceId::Route { destination, hops } => {
                route(destination).filter(|current: &Revisions| current == hops)
            }
            ResourceId::Session { .. } | ResourceId::Mcp { .. } => Some(Vec::new()),
        }
    }

    /// Whether every target in `revisions` is still at its revision.
    pub(crate) fn current(&self, revisions: &[(TargetName, u64)]) -> bool {
        (revisions.iter()).all(|(name, revision)| {
            (self.entries.get(name)).is_some_and(|target| target.revision == *revision)
        })
    }
}

/// Whether `redefined`, as a redefinition saves them, replaces any of `revisions`.
pub(crate) fn replaces(redefined: &[TargetDefinition], revisions: &[(TargetName, u64)]) -> bool {
    (revisions.iter()).any(|(name, revision)| {
        (redefined.iter()).any(|target| &target.name == name && *revision < target.revision)
    })
}

/// Every target is valid, and has a route.
fn validate_graph(entries: &Entries) -> Result<(), TargetError> {
    for (name, target) in entries {
        target.validate()?;
        route(entries, name, &mut BTreeSet::new())?;
    }
    validate_passwords(entries)
}

/// Every password a definition gives is admitted for the machine that reads it, the
/// target's origin (root is this host); one whose origin is not yet defined is
/// admitted once it is.
pub(super) fn validate_passwords(entries: &Entries) -> Result<(), TargetError> {
    for target in entries.values() {
        let reader = match &target.origin {
            None => Some(skyhook_worker::target::Platform::current().os),
            Some(origin) => entries.get(origin).map(|origin| origin.os),
        };
        let password = (target.connection.password()).and_then(Password::given);
        if let (Some(reader), Some(password)) = (reader, password) {
            let transport = target.connection.kind();
            (password.admit(reader))
                .map_err(|problem| TargetError::InvalidPassword { transport, problem })?;
        }
    }
    Ok(())
}

/// The route to `name`, which no target on it already `visited`. Jumps belong to the
/// connection their destination's origin starts, so a jump's key paths and agent are
/// never reinterpreted on another machine: each is an SSH target of that origin. That
/// connection's one terminal shows every prompt, so at most one of its targets has a
/// secret: two could be told apart only by reading their prompts.
fn route<'a>(
    entries: &'a Entries,
    name: &'a TargetName,
    visited: &mut BTreeSet<&'a TargetName>,
) -> Result<Route, TargetError> {
    let destination = (entries.get(name)).ok_or_else(|| TargetError::Unknown(name.clone()))?;
    visited.insert(name);
    let mut reference = |from: &TargetDefinition, edge, reference: &'a TargetName| {
        if !visited.insert(reference) {
            return Err(TargetError::Cycle(reference.clone()));
        }
        (entries.get(reference)).ok_or_else(|| TargetError::UnknownReference {
            target: from.name.clone(),
            edge,
            reference: reference.clone(),
        })
    };
    let origin_of = |target: &TargetDefinition| {
        (target.origin.clone()).map_or(TargetRef::Root, TargetRef::Named)
    };
    let mut jumps = Vec::new();
    let mut from = destination;
    while let Some(via) = &from.via {
        let jump = reference(from, TargetEdge::Via, via)?;
        let ssh = jump
            .connection
            .ssh()
            .ok_or_else(|| TargetError::WinrmJump {
                target: name.clone(),
                jump: jump.name.clone(),
            })?;
        if jump.origin != destination.origin {
            return Err(TargetError::OriginMismatch {
                target: name.clone(),
                jump: jump.name.clone(),
                origin: origin_of(destination),
                jump_origin: origin_of(jump),
            });
        }
        jumps.push(SshJump {
            name: jump.name.clone(),
            revision: jump.revision,
            host: jump.host.clone(),
            ssh: ssh.clone(),
        });
        from = jump;
    }
    jumps.reverse();
    let ssh = (jumps.iter()).map(|jump| (&jump.name, &jump.ssh));
    let connection = ssh.chain(destination.connection.ssh().map(|ssh| (name, ssh)));
    let secrets: Vec<_> = (connection.filter(|(_, ssh)| ssh.auth.password().is_some()))
        .map(|(name, _)| name)
        .collect();
    if let [first, second, ..] = secrets[..] {
        return Err(TargetError::SecretsInOneConnection {
            target: name.clone(),
            first: first.clone(),
            second: second.clone(),
        });
    }
    let origin = match &destination.origin {
        None => None,
        Some(origin) => {
            reference(destination, TargetEdge::Origin, origin)?;
            Some(Box::new(self::route(entries, origin, visited)?))
        }
    };
    Ok(Route {
        origin,
        jumps,
        destination: destination.clone(),
    })
}

/// Validate known route edges while permitting references supplied at runtime.
pub(super) fn validate_route_cycles(entries: &Entries) -> Result<(), TargetError> {
    for name in entries.keys() {
        let mut current = name;
        let mut visited = BTreeSet::new();
        while let Some(target) = entries.get(current) {
            if !visited.insert(current) {
                return Err(TargetError::Cycle(current.clone()));
            }
            let Some(parent) = target.via.as_ref().or(target.origin.as_ref()) else {
                break;
            };
            current = parent;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::ConnectionKind;
    use skyhook_tools::error::AdmissionError;
    use skyhook_worker::diagnostic::Subject;
    use std::assert_matches;

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        crate::target::tests::definition(name, ".", via)
    }

    fn name(name: &str) -> TargetName {
        name.parse().unwrap()
    }

    fn names(route: &Route) -> Vec<String> {
        (route.identity().into_iter())
            .map(|(name, _)| name.into())
            .collect()
    }

    #[test]
    fn routes_are_outermost_first_and_cycles_are_rejected() {
        let registry = TargetRegistry::from_definitions([
            target("edge", None),
            target("bastion", Some("edge")),
            target("build", Some("bastion")),
        ])
        .unwrap();
        let route = registry.route(&name("build")).unwrap();
        assert_eq!(names(&route), ["edge", "bastion", "build"]);
        let mut changed = target("edge", Some("build"));
        changed.source = TargetSource::Session;
        let result = registry.upsert(changed);
        assert_matches!(result, Err(TargetError::Cycle(_)));
    }

    #[test]
    fn missing_route_references_identify_the_edge() {
        let registry = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        for edge in [TargetEdge::Via, TargetEdge::Origin] {
            let mut invalid = target("invalid", None);
            match edge {
                TargetEdge::Via => invalid.via = Some(name("missing")),
                TargetEdge::Origin => invalid.origin = Some(name("missing")),
            }
            let result = registry.upsert(invalid);
            assert_matches!(result, Err(TargetError::UnknownReference {
                target,
                edge: actual,
                reference,
            }) if target == name("invalid") && actual == edge && reference == name("missing"));
        }
        assert_matches!(
            registry.route(&name("absent")),
            Err(TargetError::Unknown(unknown)) if unknown == name("absent")
        );
    }

    /// Config and `target_add` alike refuse two secrets in one SSH connection, which
    /// an origin between them splits in two.
    #[test]
    fn one_ssh_connection_types_at_most_one_secret() {
        let secret = |name: &str, via: Option<&str>| {
            let mut target = target(name, via);
            let password = skyhook_worker::secret::Credential::Literal("pw".into());
            let password = Password::Given(password);
            target.ssh_mut().auth = crate::target::SshAuth::Password { password };
            target
        };
        let two = [secret("gateway", None), secret("build", Some("gateway"))];
        let Err(error) = TargetRegistry::from_definitions(two) else {
            panic!("two secrets were accepted");
        };
        assert_matches!(&error, TargetError::SecretsInOneConnection { target, first, second }
            if *target == name("build") && *first == name("gateway") && *second == name("build"));
        assert!(error.to_string().contains("with `origin` instead of `via`"));
        let registry = TargetRegistry::from_definitions([secret("gateway", None)]).unwrap();
        let added = registry.upsert(secret("build", Some("gateway")));
        assert_matches!(added, Err(TargetError::SecretsInOneConnection { .. }));
        registry.upsert(target("build", Some("gateway"))).unwrap();
        let mut split = secret("build", None);
        split.origin = Some(name("gateway"));
        registry.upsert(split).unwrap();
    }

    #[test]
    fn jumps_share_the_origin_that_starts_their_connection() {
        let shim = target("shim", None);
        let mut jump = target("jump", None);
        jump.origin = Some(name("shim"));
        let mut destination = target("destination", Some("jump"));
        let mismatched = [shim.clone(), jump.clone(), destination.clone()];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == name("shim").into()
        ));
        // A jump reached from root cannot continue a connection started on shim.
        let mut remote = target("remote", Some("root-jump"));
        remote.origin = Some(name("shim"));
        let mismatched = [shim.clone(), target("root-jump", None), remote];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch {
                jump_origin: TargetRef::Root,
                ..
            })
        ));
        destination.origin = Some(name("shim"));
        // Origins nest: deep's connection starts on destination, itself reached from shim.
        let mut deep = target("deep", None);
        deep.origin = Some(name("destination"));
        let registry = TargetRegistry::from_definitions([shim, jump, destination, deep]).unwrap();
        let route = registry.route(&name("deep")).unwrap();
        assert_eq!(names(&route), ["shim", "jump", "destination", "deep"]);
        // Each connection's route nests the route to the origin that starts it, and
        // keeps the jumps of the SSH process that origin starts.
        let shape = |route: &Route| {
            let jumps: Vec<_> = route
                .jumps
                .iter()
                .map(|jump| jump.name.to_string())
                .collect();
            (route.destination.name.to_string(), jumps)
        };
        let destination = route.origin.as_deref().unwrap();
        let shim = destination.origin.as_deref().unwrap();
        assert_eq!(shape(&route), ("deep".into(), vec![]));
        assert_eq!(
            shape(destination),
            ("destination".into(), vec!["jump".into()])
        );
        assert_eq!(
            (shape(shim), &shim.origin),
            (("shim".into(), vec![]), &None)
        );
        let from_config = |config: serde_json::Value| {
            let config = serde_json::from_value(config).unwrap();
            TargetDefinition::from_config("h".into(), config)
        };
        let via_origin = from_config(
            serde_json::json!({"type": "ssh", "host": "h", "via": "shim", "origin": "shim"}),
        );
        assert_matches!(via_origin, Err(TargetError::ViaIsOrigin(_)));
        // `origin: root` spells the default; root is never a jump.
        let from_root =
            from_config(serde_json::json!({"type": "ssh", "host": "h", "origin": "root"}));
        assert_eq!(from_root.unwrap().origin, None);
        let via_root = from_config(serde_json::json!({"type": "ssh", "host": "h", "via": "root"}));
        assert_matches!(via_root, Err(TargetError::RootCannotBeJump));
        // A bad link is the link's fault, not the valid name's.
        for edge in [TargetEdge::Origin, TargetEdge::Via] {
            let bad = from_config(
                serde_json::json!({"type": "ssh", "host": "h", edge.to_string(): "bad target"}),
            );
            let error = bad.unwrap_err();
            assert_matches!(
                error,
                TargetError::InvalidReference { edge: found } if found == edge
            );
            let diagnostic = AdmissionError::from(error).diagnostic();
            assert_eq!(
                diagnostic.context.subject,
                Subject::argument([edge.to_string()])
            );
        }
    }

    /// A WinRM target connects straight from its origin, any target, and is never a
    /// jump; its settings go with its type alone, and its password is admitted as SSH's.
    #[test]
    fn winrm_targets_connect_from_an_origin_and_are_never_jumps() {
        use crate::target::tests::winrm;
        let registry =
            TargetRegistry::from_definitions([target("gateway", None), winrm("win", None)])
                .unwrap();
        let mut reached = winrm("inner", Some("win"));
        registry.upsert(reached.clone()).unwrap();
        reached.via = Some(name("gateway"));
        assert_matches!(
            registry.upsert(reached),
            Err(TargetError::WinrmVia(target)) if target == name("inner")
        );
        let mut behind = target("behind", Some("win"));
        assert_matches!(
            registry.upsert(behind.clone()),
            Err(TargetError::WinrmJump { jump, .. }) if jump == name("win")
        );
        (behind.via, behind.origin) = (None, Some(name("win")));
        registry.upsert(behind).unwrap();

        let config = |config: serde_json::Value| {
            let config = serde_json::from_value(config).unwrap();
            TargetDefinition::from_config("h".into(), config)
        };
        let settings =
            serde_json::json!({"user": "u", "password": " ", "auth": "basic", "tls": "off"});
        assert_matches!(
            config(serde_json::json!({"type": "winrm", "host": "h"})),
            Err(TargetError::MissingSettings(ConnectionKind::Winrm))
        );
        assert_matches!(
            config(serde_json::json!({"type": "ssh", "host": "h", "winrm": settings})),
            Err(TargetError::ForeignSettings {
                settings: ConnectionKind::Winrm,
                ..
            })
        );
        assert_matches!(
            config(
                serde_json::json!({"type": "winrm", "host": "h", "winrm": settings,
                "ssh": {"user": "u"}})
            ),
            Err(TargetError::ForeignSettings {
                settings: ConnectionKind::Ssh,
                ..
            })
        );
        let blank = config(serde_json::json!({"type": "winrm", "host": "h", "winrm": settings}));
        let error = registry.upsert(blank.unwrap()).unwrap_err();
        let diagnostic = AdmissionError::from(error).diagnostic();
        assert_eq!(
            diagnostic.context.subject,
            Subject::argument(["winrm", "password"])
        );
        let windows = winrm("w", None);
        assert_eq!(windows.os, skyhook_worker::target::Os::Windows);
    }

    /// A password file is judged by the rules of the origin that reads it, root's by
    /// this host's, and refused before anything reads it.
    #[test]
    fn password_files_are_absolute_by_their_origins_rules() {
        use skyhook_worker::secret::SourceProblem;
        let definition = |name: &str, config| {
            let config = serde_json::from_value(config).unwrap();
            TargetDefinition::from_config(name.into(), config).unwrap()
        };
        let windows = serde_json::json!({"type": "ssh", "host": "w", "os": "windows"});
        let registry = TargetRegistry::from_definitions([definition("windows", windows)]).unwrap();
        let reading = |origin: Option<&str>, file: &str| {
            let target = serde_json::json!({"type": "ssh", "host": "h", "origin": origin,
                "ssh": {"password": {"file": file}}});
            registry.upsert(definition("t", target))
        };
        for (origin, file, admitted) in [
            (None, "/etc/pw", true),
            (None, r"C:\pw", false),
            (Some("windows"), r"C:\pw", true),
            (Some("windows"), "/etc/pw", false),
        ] {
            match reading(origin, file) {
                Ok(_) => assert!(admitted, "{file} from {origin:?}"),
                Err(TargetError::InvalidPassword {
                    problem: SourceProblem::RelativeFile,
                    ..
                }) => assert!(!admitted, "{file} from {origin:?}"),
                Err(error) => panic!("{file} from {origin:?}: {error}"),
            }
        }
    }
}
