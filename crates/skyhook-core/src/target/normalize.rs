//! Resolve target metadata and translate SSH jump routes before registry publication.
use super::{
    ROOT_TARGET, SshOptions, TargetConfig, TargetConfigType, TargetDefinition, TargetError,
    TargetSource,
};
use crate::remote::ssh::ResolvedSsh;
use futures_util::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[async_trait::async_trait]
pub(crate) trait ConfigResolver: Send + Sync {
    async fn resolve(&self, target: &TargetDefinition) -> Result<ResolvedSsh, TargetError>;
}
pub(crate) struct LocalResolver;
#[async_trait::async_trait]
impl ConfigResolver for LocalResolver {
    async fn resolve(&self, target: &TargetDefinition) -> Result<ResolvedSsh, TargetError> {
        crate::remote::backend::resolve_local(target)
            .await
            .map_err(|e| TargetError::Import(e.to_string()))
    }
}

/// Check route cycles that can be established without resolving SSH metadata.
/// Unknown jumps terminate a partial route, rather than rejecting a config layer.
pub(crate) fn validate_static_routes(
    definitions: Vec<TargetDefinition>,
) -> Result<(), TargetError> {
    let entries = definitions
        .into_iter()
        .map(|mut target| {
            if target.via.is_none() {
                target.via = default_via(&target);
            }
            (target.name.clone(), target)
        })
        .collect();
    super::registry::validate_route_cycles(&entries)
}

// SSH ProxyJump may insert hops before this anchor, but root is implicit and
// never a jump. An explicit via replaces this default, not adds another edge.
fn default_via(target: &TargetDefinition) -> Option<String> {
    (target.origin != ROOT_TARGET).then(|| target.origin.clone())
}

pub(crate) async fn normalize(
    definitions: Vec<TargetDefinition>,
    existing: Vec<TargetDefinition>,
    resolver: Arc<dyn ConfigResolver>,
) -> Result<Vec<TargetDefinition>, TargetError> {
    let names = definitions
        .iter()
        .map(|d| d.name.clone())
        .collect::<Vec<_>>();
    let mut state = Normalizer {
        done: existing.iter().map(|d| d.name.clone()).collect(),
        entries: existing.into_iter().map(|d| (d.name.clone(), d)).collect(),
        active: BTreeSet::new(),
        changed: BTreeSet::new(),
        resolver,
    };
    for d in definitions {
        state.done.remove(&d.name);
        state.entries.insert(d.name.clone(), d);
    }
    for name in names {
        state.expand(name).await?;
    }
    Ok(state
        .changed
        .into_iter()
        .map(|name| state.entries.remove(&name).expect("expanded entry"))
        .collect())
}
struct Normalizer {
    entries: BTreeMap<String, TargetDefinition>,
    done: BTreeSet<String>,
    active: BTreeSet<String>,
    changed: BTreeSet<String>,
    resolver: Arc<dyn ConfigResolver>,
}
impl Normalizer {
    fn expand(&mut self, name: String) -> BoxFuture<'_, Result<(), TargetError>> {
        Box::pin(async move {
            if self.done.contains(&name) {
                return Ok(());
            }
            if self.active.len() >= 128 {
                return Err(TargetError::Import(
                    "SSH jump chain exceeds 128 hops".into(),
                ));
            }
            if !self.active.insert(name.clone()) {
                return Err(TargetError::Cycle(name));
            }
            let mut target = self
                .entries
                .get(&name)
                .cloned()
                .ok_or_else(|| TargetError::UnknownJump(name.clone()))?;
            let resolved = self.resolver.resolve(&target).await?;
            target.host = resolved.host.clone();
            if target.via.is_none() {
                let mut previous = default_via(&target);
                if let Some(jumps) = resolved.proxy_jump.as_deref().filter(|s| *s != "none") {
                    let jumps = expand_jump_tokens(jumps, &target.ssh_alias, &resolved);
                    for (index, token) in jumps.split(',').enumerate() {
                        let jump = Jump::parse(token)?;
                        let candidate = self
                            .entries
                            .get(&jump.host)
                            .cloned()
                            .filter(|d| d.origin == target.origin);
                        let mut hop = if let Some(candidate) = candidate {
                            candidate
                        } else {
                            let mut d = TargetDefinition::from_config(
                                "imported-hop".into(),
                                TargetConfig {
                                    r#type: TargetConfigType::Ssh,
                                    host: jump.host.clone(),
                                    workspace: ".".into(),
                                    via: None,
                                    ssh: SshOptions::default(),
                                },
                                TargetSource::SshConfig,
                            )?;
                            d.origin.clone_from(&target.origin);
                            d
                        };
                        if let Some(user) = jump.user {
                            hop.ssh.user = Some(user);
                        }
                        if let Some(port) = jump.port {
                            hop.ssh.port = Some(port);
                        }
                        if index > 0 {
                            hop.via.clone_from(&previous);
                        }
                        let reusable = self.entries.get(&jump.host).is_some_and(|old| {
                            old.origin == hop.origin && old.ssh == hop.ssh && old.via == hop.via
                        });
                        let hop_name = if reusable {
                            jump.host
                        } else {
                            let identity = serde_json::to_vec(&(
                                &hop.origin,
                                &hop.ssh_alias,
                                &hop.ssh,
                                &hop.via,
                            ))
                            .expect("serializable hop");
                            let key = crate::sha256_hex(&identity);
                            let base = format!("ssh-hop-{}", &key[..20]);
                            hop.generated_key = Some(key);
                            let mut name = base.clone();
                            let mut suffix = 0;
                            while self.entries.get(&name).is_some_and(|old| {
                                old.origin != hop.origin
                                    || old.ssh_alias != hop.ssh_alias
                                    || old.ssh != hop.ssh
                                    || old.generated_key != hop.generated_key
                            }) {
                                suffix += 1;
                                name = format!("{base}-{suffix}");
                            }
                            hop.name.clone_from(&name);
                            self.entries.entry(name.clone()).or_insert(hop);
                            name
                        };
                        self.expand(hop_name.clone()).await?;
                        previous = Some(hop_name);
                    }
                }
                target.via = previous;
            }
            target.resolved = Some(resolved);
            self.entries.insert(name.clone(), target);
            self.active.remove(&name);
            self.done.insert(name.clone());
            self.changed.insert(name);
            Ok(())
        })
    }
}

fn expand_jump_tokens(value: &str, alias: &str, resolved: &ResolvedSsh) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            result.push(character);
            continue;
        }
        match chars.next() {
            Some('%') => result.push('%'),
            Some('h') => result.push_str(&resolved.host),
            Some('n') => result.push_str(alias),
            Some('p') => result.push_str(&resolved.port.to_string()),
            Some('r') => result.push_str(&resolved.user),
            Some(other) => {
                result.push('%');
                result.push(other);
            }
            None => result.push('%'),
        }
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
struct Jump {
    host: String,
    user: Option<String>,
    port: Option<u16>,
}
impl Jump {
    fn parse(value: &str) -> Result<Self, TargetError> {
        let value = value.strip_prefix("ssh://").unwrap_or(value);
        let (user, endpoint) = value
            .rsplit_once('@')
            .map_or((None, value), |(u, h)| (Some(u.to_owned()), h));
        let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| TargetError::Import("invalid IPv6 jump".into()))?;
            (
                host,
                if tail.is_empty() {
                    None
                } else {
                    Some(
                        tail.strip_prefix(':')
                            .ok_or_else(|| TargetError::Import("invalid jump port".into()))?,
                    )
                },
            )
        } else {
            endpoint
                .rsplit_once(':')
                .map_or((endpoint, None), |(h, p)| (h, Some(p)))
        };
        if host.is_empty() || host.contains('/') || user.as_deref() == Some("") {
            return Err(TargetError::Import("invalid SSH jump destination".into()));
        }
        let port = port
            .map(|p| {
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| TargetError::Import("invalid SSH jump port".into()))
            })
            .transpose()?;
        Ok(Self {
            host: host.into(),
            user,
            port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FixtureResolver(BTreeMap<String, Option<String>>);
    #[async_trait::async_trait]
    impl ConfigResolver for FixtureResolver {
        async fn resolve(&self, target: &TargetDefinition) -> Result<ResolvedSsh, TargetError> {
            Ok(ResolvedSsh {
                host: format!("{}.internal", target.ssh_alias),
                user: target.ssh.user.clone().unwrap_or("default".into()),
                port: target.ssh.port.unwrap_or(22),
                identity_files: vec!["~/.ssh/secret".into()],
                options: BTreeMap::new(),
                proxy_jump: self.0.get(&target.ssh_alias).cloned().flatten(),
                proxy_command: None,
            })
        }
    }
    fn definition(name: &str) -> TargetDefinition {
        TargetDefinition::from_config(
            name.into(),
            serde_json::from_value(serde_json::json!({"type":"ssh","host":name})).unwrap(),
            TargetSource::SshConfig,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn static_cycles_follow_normalized_via_and_origin_routes() {
        use super::super::TargetRegistry;

        let cases = [
            // Root is implicit, not a self-edge.
            vec![("a", ROOT_TARGET, None), ("b", ROOT_TARGET, Some("a"))],
            vec![("a", ROOT_TARGET, Some("a"))],
            vec![("a", ROOT_TARGET, Some("b")), ("b", ROOT_TARGET, Some("a"))],
            // An omitted via anchors the target on its non-root origin.
            vec![("a", "b", None), ("b", ROOT_TARGET, None)],
            vec![("a", "a", None)],
            vec![("a", "b", None), ("b", "a", None)],
            vec![("a", "b", None), ("b", ROOT_TARGET, Some("a"))],
            // Explicit via wins: do not invent a direct a -> origin edge.
            // Runtime rejects this as an invalid origin, NOT a route cycle.
            vec![("a", "a", Some("b")), ("b", ROOT_TARGET, None)],
        ];
        for case in cases {
            let definitions = case
                .iter()
                .map(|(name, origin, via)| {
                    let mut target = definition(name);
                    target.origin = (*origin).into();
                    target.via = via.map(str::to_owned);
                    target
                })
                .collect::<Vec<_>>();
            let static_result = validate_static_routes(definitions.clone());
            let normalized = normalize(
                definitions,
                vec![],
                Arc::new(FixtureResolver(BTreeMap::new())),
            )
            .await
            .unwrap();
            let runtime = TargetRegistry::from_definitions(normalized);
            assert_eq!(
                matches!(static_result, Err(TargetError::Cycle(_))),
                matches!(runtime, Err(TargetError::Cycle(_))),
                "{case:?}"
            );
            assert!(static_result.is_ok() || matches!(static_result, Err(TargetError::Cycle(_))));
        }
    }

    #[test]
    fn unknown_origins_and_jumps_do_not_hide_other_static_cycles() {
        let mut unknown_origin = definition("a");
        unknown_origin.origin = "later-origin".into();
        let mut unknown_jump = definition("b");
        unknown_jump.via = Some("later-jump".into());
        let partial = vec![unknown_origin, unknown_jump];
        validate_static_routes(partial.clone()).unwrap();
        let mut cyclic = definition("z");
        cyclic.via = Some("z".into());
        assert!(matches!(
            validate_static_routes(partial.into_iter().chain([cyclic]).collect()),
            Err(TargetError::Cycle(name)) if name == "z"
        ));
    }

    #[tokio::test]
    async fn remote_origins_anchor_automatic_jump_chains() {
        let mut destination = definition("dest");
        destination.origin = "remote".into();
        let existing = vec![definition("remote")];
        let resolver = Arc::new(FixtureResolver(BTreeMap::from([(
            "dest".into(),
            Some("unnamed".into()),
        )])));
        let imported = normalize(vec![destination], existing.clone(), resolver)
            .await
            .unwrap();
        assert!(imported.iter().all(|target| target.origin == "remote"));
        let registry =
            super::super::TargetRegistry::from_definitions(existing.into_iter().chain(imported))
                .unwrap();
        let route = registry.route("dest").await.unwrap();
        assert_eq!(route[0].name, "remote");
        assert_eq!(route[1].ssh_alias, "unnamed");
        assert_eq!(route[2].name, "dest");
    }
    #[tokio::test]
    async fn recursive_jump_cycles_are_rejected() {
        let resolver = Arc::new(FixtureResolver(BTreeMap::from([
            ("one".into(), Some("two".into())),
            ("two".into(), Some("one".into())),
        ])));
        assert!(matches!(
            normalize(vec![definition("one"), definition("two")], vec![], resolver).await,
            Err(TargetError::Cycle(_))
        ));
    }
    #[tokio::test]
    async fn first_remote_jump_retains_its_own_jump_chain() {
        let mut destination = definition("dest");
        destination.origin = "remote".into();
        let resolver = Arc::new(FixtureResolver(BTreeMap::from([
            ("dest".into(), Some("inner".into())),
            ("inner".into(), Some("outer".into())),
        ])));
        let existing = vec![definition("remote")];
        let definitions = normalize(vec![destination], existing.clone(), resolver)
            .await
            .unwrap();
        let registry =
            super::super::TargetRegistry::from_definitions(existing.into_iter().chain(definitions))
                .unwrap();
        let route = registry.route("dest").await.unwrap();
        assert_eq!(
            route
                .iter()
                .map(|target| target.ssh_alias.as_str())
                .collect::<Vec<_>>(),
            ["remote", "outer", "inner", "dest"]
        );
    }
}
