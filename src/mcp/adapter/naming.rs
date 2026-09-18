//! Stable, bounded MCP registry names with catalog-wide collision avoidance.
use super::super::manager::DiscoveredTool;
use std::collections::{BTreeMap, BTreeSet};

/// Prefer readable names. Allocate against the whole catalog before registration
/// so ambiguous server/tool boundaries never depend on discovery order. Natural
/// names take precedence over generated suffixes, even if discovered later.
pub(super) fn tool_names(
    catalog: &[DiscoveredTool],
    is_registered: impl Fn(&str) -> bool,
) -> Vec<String> {
    let raw: Vec<_> = catalog
        .iter()
        .map(|item| format!("mcp_{}_{}", item.server, item.tool.name))
        .collect();
    let safe = |name: &str| {
        name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let mut counts = BTreeMap::new();
    for name in &raw {
        *counts.entry(name.as_str()).or_insert(0_usize) += 1;
    }
    let reserved: BTreeSet<_> = raw
        .iter()
        .filter(|name| safe(name))
        .map(String::as_str)
        .collect();
    let mut order: Vec<_> = (0..catalog.len()).collect();
    order.sort_by(|&left, &right| {
        (&catalog[left].server, &catalog[left].tool.name)
            .cmp(&(&catalog[right].server, &catalog[right].tool.name))
    });
    let mut names = vec![String::new(); catalog.len()];
    let mut used = BTreeSet::new();
    for index in order {
        let name = &raw[index];
        if safe(name) && counts[name.as_str()] == 1 && !is_registered(name) {
            names[index] = name.clone();
            used.insert(name.clone());
            continue;
        }
        let item = &catalog[index];
        let server = item.server.as_str();
        let tool = item.tool.name.as_ref();
        // Length-delimited originals distinguish identities such as a_b/c and
        // a/b_c, as well as names that normalize to the same ASCII spelling.
        let hash = crate::sha256_hex(format!("{}:{server}{}:{tool}", server.len(), tool.len()));
        let readable: String = name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        for attempt in 0_usize.. {
            let suffix = if attempt == 0 {
                hash[..8].to_owned()
            } else {
                format!("{}_{attempt}", &hash[..8])
            };
            let prefix = &readable[..readable.len().min(64 - 1 - suffix.len())];
            let candidate = format!("{prefix}_{suffix}");
            if !reserved.contains(candidate.as_str())
                && !used.contains(&candidate)
                && !is_registered(&candidate)
            {
                used.insert(candidate.clone());
                names[index] = candidate;
                break;
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn discovered(server: &str, tool: &str) -> DiscoveredTool {
        DiscoveredTool {
            server: server.to_owned(),
            tool: serde_json::from_value(json!({"name":tool,"inputSchema":{"type":"object"}}))
                .unwrap(),
            capabilities: Vec::new(),
        }
    }

    fn tool_name(item: &DiscoveredTool) -> String {
        tool_names(std::slice::from_ref(item), |_| false).remove(0)
    }

    #[test]
    fn names_are_stable_safe_bounded_and_collision_resistant() {
        assert_eq!(
            tool_name(&discovered("docs-v2", "Search")),
            "mcp_docs-v2_Search"
        );
        assert_eq!(tool_name(&discovered("s", &"x".repeat(58))).len(), 64);
        let long = "x".repeat(500);
        let identities = [
            ("a-b", "c"),
            ("a_b", "c"),
            ("a", "b_c"),
            ("a_b", "c_"),
            ("👋", "工具"),
            ("", ""),
            ("server", "background"),
            (long.as_str(), "first"),
            (long.as_str(), "second"),
        ];
        let mut catalog: Vec<_> = identities
            .iter()
            .map(|(server, tool)| discovered(server, tool))
            .collect();
        let names = tool_names(&catalog, |_| false);
        assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
        for name in &names {
            assert!(name.starts_with("mcp_"));
            assert!(name.len() <= 64);
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            );
        }
        // Both ambiguous identities receive suffixes, rather than first-one-wins.
        assert!(names[1].starts_with("mcp_a_b_c_"));
        assert!(names[2].starts_with("mcp_a_b_c_"));
        assert_ne!(names[1], names[2]);
        assert_eq!(names[7].len(), 64);
        assert_eq!(names[8].len(), 64);
        catalog.reverse();
        let mut reversed = tool_names(&catalog, |_| false);
        reversed.reverse();
        assert_eq!(names, reversed);
        // Adding unrelated names does not rename existing tools.
        catalog.reverse();
        catalog.push(discovered("unrelated", "new_tool"));
        assert_eq!(&tool_names(&catalog, |_| false)[..names.len()], names);
    }

    #[test]
    fn natural_names_take_precedence_over_generated_suffixes() {
        let invalid = discovered("bad.server", "write");
        let generated = tool_name(&invalid);
        let natural = discovered(
            "bad_server",
            generated.strip_prefix("mcp_bad_server_").unwrap(),
        );
        let names = tool_names(&[invalid.clone(), natural.clone()], |_| false);
        assert_eq!(names[1], generated);
        assert_ne!(names[0], names[1]);
        assert_eq!(names[0], format!("{generated}_1"));
        let reverse = tool_names(&[natural, invalid], |_| false);
        assert_eq!(names, vec![reverse[1].clone(), reverse[0].clone()]);
    }

    #[test]
    fn registered_names_and_their_fallbacks_are_reserved() {
        let item = discovered("filesystem", "write_file");
        let raw = tool_name(&item);
        let fallback = tool_names(std::slice::from_ref(&item), |name| name == raw).remove(0);
        let reserved = [raw, fallback.clone()];
        let name = tool_names(&[item], |name| reserved.iter().any(|old| old == name)).remove(0);
        assert!(!reserved.contains(&name));
        assert_eq!(name, format!("{fallback}_1"));
    }
}
