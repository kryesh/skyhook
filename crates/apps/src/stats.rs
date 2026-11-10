//! Read-only session reporting: aligned or Markdown tables, a `tree`-style listing, or JSON.

use crate::args::StatsFormat;
use crate::text::{brief, duration, grouped};
use chrono::{DateTime, Utc};
use skyhook::identity::SessionId;
use skyhook::product::text::Table;
use skyhook::{
    provider::protocol::UsageTotals,
    session::{
        SessionError, SessionStore,
        stats::{AgentStats, RequestStats, SessionStats},
    },
};
use skyhook_tools::diagnostic::escape_controls;
use std::{
    fmt::Write as _,
    io::Write,
    path::{Path, PathBuf},
};

type Error = Box<dyn std::error::Error>;

/// What `stats` reports on.
#[derive(Debug, PartialEq, Eq)]
pub enum StatsTarget {
    /// The workspace's sessions with their totals.
    List,
    /// One session, as the tables for reading when no format is named.
    Session(SessionId, Option<StatsFormat>),
}

pub struct StatsRequest {
    pub target: StatsTarget,
    /// The workspace whose session history holds the sessions.
    pub workspace: PathBuf,
}

pub async fn run(
    request: StatsRequest,
    identity: &skyhook::product::Identity,
    output: &mut impl Write,
) -> Result<(), Error> {
    let workspace = tokio::fs::canonicalize(&request.workspace)
        .await
        .map_err(|error| format!("workspace {}: {error}", request.workspace.display()))?;
    let sessions = skyhook::config::workspace_session_root(&workspace);
    let (session, format) = match request.target {
        StatsTarget::List => {
            let listed = list(&sessions, identity.name()).await?;
            return Ok(output.write_all(listing(&listed).as_bytes())?);
        }
        StatsTarget::Session(session, format) => (session, format),
    };
    let stats = SessionStore::stats(&sessions, session)
        .await
        .map_err(|error| match error {
            SessionError::NotFound(_) => {
                format!("session {session} not found under {}", sessions.display())
            }
            error => error.to_string(),
        })?;
    match format {
        None => output.write_all(tables(&stats, false).as_bytes())?,
        Some(StatsFormat::Markdown) => output.write_all(tables(&stats, true).as_bytes())?,
        Some(StatsFormat::Tree) => output.write_all(tree(&stats).as_bytes())?,
        Some(StatsFormat::Json) => {
            serde_json::to_writer(&mut *output, &stats)?;
            output.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Every session of the current format in the history, newest first. Earlier formats
/// are left out; a session that should be readable but is not is reported.
async fn list(sessions: &Path, name: &str) -> Result<Vec<SessionStats>, Error> {
    let read =
        |session: SessionId| async move { (session, SessionStore::stats(sessions, session).await) };
    let reads = skyhook::session::SessionStore::read_each(sessions, read).await;
    let reads = reads.map_err(|error| format!("{}: {error}", sessions.display()))?;
    let mut listed = Vec::new();
    for (session, result) in reads {
        match result {
            Ok(stats) => listed.push(stats),
            Err(SessionError::UnsupportedVersion(_)) => {}
            Err(SessionError::NotFound(_)) => {}
            Err(error) => eprintln!("{name} stats: {session}: {}", escape_controls(error)),
        }
    }
    listed.sort_by_key(|stats| std::cmp::Reverse(stats.started));
    Ok(listed)
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A prompt on one line: whitespace runs collapsed, other controls escaped, at most
/// `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    escape_controls(brief(text, limit))
}

/// Input, cached, written and output token counts.
fn usage_cells(usage: &UsageTotals) -> [String; 4] {
    [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.output_tokens,
    ]
    .map(grouped)
}

/// Completed over requested model calls.
fn ratio(requests: &RequestStats) -> String {
    format!("{}/{}", requests.completed, requests.requested)
}

/// Time from the agent's start to its final completion; unfinished agents show none.
fn elapsed(agent: &AgentStats) -> String {
    agent
        .outcome
        .finished_at()
        .map_or_else(|| "—".into(), |finished| duration(finished - agent.started))
}

fn tool_calls(agent: &AgentStats) -> u64 {
    agent.tools.values().map(|tool| tool.calls).sum()
}

fn tables(stats: &SessionStats, markdown: bool) -> String {
    let mut output = String::new();
    let totals = &stats.totals;
    let (heading, bullet, section) = if markdown {
        ("# ", "- ", "\n## ")
    } else {
        ("", "", "\n")
    };
    let _ = writeln!(output, "{heading}Session {}\n", stats.session);
    let _ = writeln!(
        output,
        "{bullet}Host: {}",
        one_line(stats.host.as_str(), usize::MAX)
    );
    if let Some(prompt) = &stats.initial_prompt {
        let _ = writeln!(
            output,
            "{bullet}Initial prompt: {}",
            one_line(prompt, usize::MAX)
        );
    }
    let _ = writeln!(
        output,
        "{bullet}Started: {} · Duration: {}",
        timestamp(stats.started),
        duration(stats.finished - stats.started)
    );
    let _ = writeln!(
        output,
        "{bullet}Agents: {} · Requests: {}/{} completed, {} failed, {} interrupted, {} retries · Compactions: {}",
        totals.agents,
        totals.requests.completed,
        totals.requests.requested,
        totals.requests.failed,
        totals.requests.interrupted,
        totals
            .requests
            .attempts
            .saturating_sub(totals.requests.requested),
        totals.compactions.completed,
    );
    let [input, cached, written, out] = usage_cells(&totals.usage);
    let _ = writeln!(
        output,
        "{bullet}Tokens: {input} input, {cached} cached, {written} written, {out} output · Tool calls: {} ({} errors)",
        grouped(totals.tool_calls.calls),
        grouped(totals.tool_calls.errors),
    );
    let mut agents = Table {
        header: &[
            "Path", "Model", "Calls", "Tools", "Input", "Cached", "Written", "Output", "Duration",
        ],
        numeric_from: 2,
        rows: stats
            .agents
            .iter()
            .map(|agent| {
                let model = agent.model.as_ref();
                let mut row = vec![
                    escape_controls(&agent.path),
                    model.map_or_else(|| "—".into(), escape_controls),
                    ratio(&agent.requests),
                    grouped(tool_calls(agent)),
                ];
                row.extend(usage_cells(&agent.usage));
                row.push(elapsed(agent));
                row
            })
            .collect(),
    };
    let mut total = vec![
        format!(
            "{} ({} agents)",
            if markdown { "**Total**" } else { "Total" },
            totals.agents
        ),
        String::new(),
        ratio(&stats.totals.requests),
        grouped(totals.tool_calls.calls),
    ];
    total.extend(usage_cells(&totals.usage));
    total.push(duration(stats.finished - stats.started));
    agents.rows.push(total);
    let models = Table {
        header: &["Model", "Calls", "Input", "Cached", "Written", "Output"],
        numeric_from: 1,
        rows: stats
            .models
            .iter()
            .map(|(name, model)| {
                let mut row = vec![escape_controls(name), ratio(&model.requests)];
                row.extend(usage_cells(&model.usage));
                row
            })
            .collect(),
    };
    let tools = Table {
        header: &["Tool", "Calls", "Errors", "Unanswered"],
        numeric_from: 1,
        rows: stats
            .tools
            .iter()
            .map(|(name, tool)| {
                vec![
                    escape_controls(name),
                    grouped(tool.calls),
                    grouped(tool.errors),
                    grouped(tool.unanswered),
                ]
            })
            .collect(),
    };
    for (title, table) in [("Agents", agents), ("Models", models), ("Tools", tools)] {
        if table.rows.is_empty() {
            continue;
        }
        let _ = writeln!(output, "{section}{title}\n");
        if markdown {
            table.markdown(&mut output);
        } else {
            crate::text::table(&table, &mut output);
        }
    }
    output
}

fn describe(agent: &AgentStats) -> String {
    let model = agent.model.as_ref().map_or_else(String::new, |model| {
        format!(" [{}]", escape_controls(model))
    });
    let [input, cached, written, out] = usage_cells(&agent.usage);
    format!(
        "{}{} · {} calls · {} tools · in {input} · cached {cached} · written {written} · out {out} · {}",
        if agent.depth == 0 { "/" } else { &agent.name },
        model,
        ratio(&agent.requests),
        grouped(tool_calls(agent)),
        elapsed(agent),
    )
}

fn listing(sessions: &[SessionStats]) -> String {
    let table = Table {
        header: &[
            "Session", "Host", "Started", "Prompt", "Agents", "Calls", "Tools", "Input", "Cached",
            "Written", "Output", "Duration",
        ],
        numeric_from: 4,
        rows: sessions
            .iter()
            .map(|stats| {
                let prompt = stats.initial_prompt.as_deref();
                let mut row = vec![
                    stats.session.to_string(),
                    one_line(stats.host.as_str(), usize::MAX),
                    timestamp(stats.started),
                    prompt.map_or_else(|| "—".into(), |prompt| one_line(prompt, 60)),
                    grouped(stats.totals.agents),
                    ratio(&stats.totals.requests),
                    grouped(stats.totals.tool_calls.calls),
                ];
                row.extend(usage_cells(&stats.totals.usage));
                row.push(duration(stats.finished - stats.started));
                row
            })
            .collect(),
    };
    let mut output = String::new();
    crate::text::table(&table, &mut output);
    output
}

/// Agents come in depth-first order, so an agent is the last of its siblings when
/// no later agent at its depth precedes one shallower than it.
fn tree(stats: &SessionStats) -> String {
    let mut output = format!(
        "{} · {}",
        stats.session,
        one_line(stats.host.as_str(), usize::MAX)
    );
    if let Some(prompt) = &stats.initial_prompt {
        let _ = write!(output, "  {}", one_line(prompt, usize::MAX));
    }
    output.push('\n');
    // Whether the ancestor at each depth was the last of its siblings.
    let mut last_at = Vec::new();
    for (index, agent) in stats.agents.iter().enumerate() {
        let last = stats.agents[index + 1..]
            .iter()
            .map(|later| later.depth)
            .take_while(|depth| *depth >= agent.depth)
            .all(|depth| depth != agent.depth);
        last_at.truncate(agent.depth);
        last_at.push(last);
        let mut line = String::new();
        for &ancestor_last in last_at.iter().take(agent.depth).skip(1) {
            line.push_str(if ancestor_last { "    " } else { "│   " });
        }
        if agent.depth > 0 {
            line.push_str(if last { "└── " } else { "├── " });
        }
        let _ = writeln!(output, "{line}{}", describe(agent));
    }
    let totals = &stats.totals;
    let [input, cached, written, out] = usage_cells(&totals.usage);
    let _ = writeln!(
        output,
        "\n{} agents, {} calls, {} tool calls, in {input}, cached {cached}, written {written}, out {out}, {}",
        totals.agents,
        ratio(&totals.requests),
        grouped(totals.tool_calls.calls),
        duration(stats.finished - stats.started),
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_guides_follow_depth_first_order() {
        let agent = |name: &str, depth: usize| AgentStats {
            path: String::new(),
            name: name.into(),
            depth,
            model: None,
            parent: None,
            owner_job: None,
            outcome: skyhook::session::stats::AgentOutcome::Running,
            started: Default::default(),
            usage: Default::default(),
            requests: Default::default(),
            compactions: Default::default(),
            tools: Default::default(),
            jobs: Default::default(),
            children: 0,
        };
        let stats = SessionStats {
            host: "other-host".parse().unwrap(),
            session: skyhook::identity::SessionId::from_bytes([1; 16]),
            initial_prompt: None,
            started: Default::default(),
            finished: Default::default(),
            agents: vec![
                agent("root", 0),
                agent("a", 1),
                agent("a1", 2),
                agent("a2", 2),
                agent("a2x", 3),
                agent("b", 1),
                agent("b1", 2),
            ],
            models: Default::default(),
            tools: Default::default(),
            totals: Default::default(),
        };
        let lines: Vec<_> = tree(&stats)
            .lines()
            .skip(1)
            .take(7)
            .map(|line| line.split(" · ").next().unwrap().to_owned())
            .collect();
        assert_eq!(
            lines,
            [
                "/",
                "├── a",
                "│   ├── a1",
                "│   └── a2",
                "│       └── a2x",
                "└── b",
                "    └── b1",
            ]
        );
    }

    /// A saved session is reported in every format and listed; a missing one is not found.
    #[tokio::test]
    async fn sessions_are_reported_in_every_format_and_listed() {
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path();
        let config = skyhook::config::Config::from_yaml(crate::tests::CONFIG).unwrap();
        let config = config.into_runtime().unwrap();
        let builder = skyhook::agent::HarnessBuilder::configured(
            &config.default_model(),
            workspace,
            "skyhook".parse().unwrap(),
        );
        let builder = builder.unwrap();
        let builder = builder.session_root(skyhook::config::workspace_session_root(workspace));
        let session = builder.build().await.unwrap().new_session().await.unwrap();
        session.run_script("return 7;".to_owned()).await.unwrap();
        session.shutdown().await.unwrap();
        let id = session.id();
        let report = async |target| {
            let request = StatsRequest {
                target,
                workspace: workspace.to_owned(),
            };
            let mut output = Vec::new();
            let identity = skyhook::product::Product::identity(&skyhook::product::Skyhook);
            let result = run(request, &identity, &mut output).await;
            result.map(|()| String::from_utf8(output).unwrap())
        };
        let session = |format| StatsTarget::Session(id, format);
        let json = report(session(Some(StatsFormat::Json))).await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["session"], id.to_string());
        assert_eq!(json["agents"][0]["model"], "test/first");
        for (format, heading) in [
            (None, format!("Session {id}\n")),
            (Some(StatsFormat::Markdown), format!("# Session {id}\n")),
            (
                Some(StatsFormat::Tree),
                format!("{id} · skyhook\n/ [test/first]"),
            ),
        ] {
            let text = report(session(format)).await.unwrap();
            assert!(text.starts_with(&heading), "{text}");
        }
        let listed = report(StatsTarget::List).await.unwrap();
        assert!(listed.lines().any(|line| line.starts_with(&id.to_string())));
        let missing = StatsTarget::Session(SessionId::from_bytes([1; 16]), None);
        let error = report(missing).await.unwrap_err().to_string();
        assert!(error.contains("not found"), "{error}");
    }
}
