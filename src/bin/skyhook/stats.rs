//! Read-only session reporting: aligned or Markdown tables, a `tree`-style listing, or JSON.

use super::{
    cli::{StatsFormat, StatsRequest},
    dump::diagnostic_text,
    tui::format::brief,
};
use chrono::{DateTime, Utc};
use skyhook::{
    identity::SessionId,
    session::{
        SessionError, SessionStore,
        stats::{AgentStats, RequestStats, SessionStats, session_stats},
    },
};
use std::{
    fmt::Write as _,
    io::{self, Write},
    path::Path,
};
use unicode_width::UnicodeWidthStr;

type Error = Box<dyn std::error::Error>;

pub(super) async fn run(request: StatsRequest) -> Result<(), Error> {
    let workspace = tokio::fs::canonicalize(&request.workspace)
        .await
        .map_err(|error| format!("workspace {}: {error}", request.workspace.display()))?;
    let sessions = workspace.join(".skyhook/sessions");
    let mut stdout = io::stdout().lock();
    let Some(session) = request.session else {
        let listed = list(&sessions).await?;
        return Ok(stdout.write_all(listing(&listed).as_bytes())?);
    };
    let stats = read(&sessions, session)
        .await
        .map_err(|error| match error {
            SessionError::Io(io) if io.kind() == io::ErrorKind::NotFound => {
                format!("session {session} not found under {}", sessions.display())
            }
            error => error.to_string(),
        })?;
    match request.format {
        None => stdout.write_all(tables(&stats, false).as_bytes())?,
        Some(StatsFormat::Markdown) => stdout.write_all(tables(&stats, true).as_bytes())?,
        Some(StatsFormat::Tree) => stdout.write_all(tree(&stats).as_bytes())?,
        Some(StatsFormat::Json) => {
            serde_json::to_writer(&mut stdout, &stats)?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}

async fn read(sessions: &Path, session: SessionId) -> Result<SessionStats, SessionError> {
    let records = SessionStore::read_records(sessions, session).await?;
    Ok(session_stats(session, &records))
}

/// Every session of the current format in the history, newest first. Earlier formats
/// are left out; a session that should be readable but is not is reported.
async fn list(sessions: &Path) -> Result<Vec<SessionStats>, Error> {
    let mut listed = Vec::new();
    let mut entries = match tokio::fs::read_dir(sessions).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(listed),
        Err(error) => return Err(format!("{}: {error}", sessions.display()).into()),
    };
    let mut ids: Vec<SessionId> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        ids.extend(
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<SessionId>().ok()),
        );
    }
    let reads = ids.iter().map(|session| read(sessions, *session));
    for (session, result) in ids.iter().zip(futures_util::future::join_all(reads).await) {
        match result {
            Ok(stats) => listed.push(stats),
            Err(SessionError::UnsupportedVersion(_)) => {}
            Err(SessionError::Io(io)) if io.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("skyhook stats: {session}: {}", diagnostic_text(error)),
        }
    }
    listed.sort_by_key(|stats| std::cmp::Reverse(stats.started));
    Ok(listed)
}

fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn duration(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    let seconds = (to - from).num_seconds().max(0);
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// A prompt on one line: whitespace runs collapsed, other controls escaped, at most
/// `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    diagnostic_text(brief(text, limit))
}

/// Completed over requested model calls.
fn ratio(requests: &RequestStats) -> String {
    format!("{}/{}", requests.completed, requests.requested)
}

/// Time from the agent's start to its final completion; unfinished agents show none.
fn elapsed(agent: &AgentStats) -> String {
    agent
        .finished
        .map_or_else(|| "—".into(), |finished| duration(agent.started, finished))
}

fn tool_calls(agent: &AgentStats) -> u64 {
    agent.tools.values().map(|tool| tool.calls).sum()
}

/// One section's rows; columns from `numeric_from` on align right.
struct Table {
    header: &'static [&'static str],
    numeric_from: usize,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn markdown(&self, output: &mut String) {
        let _ = writeln!(output, "| {} |", self.header.join(" | "));
        let rule: Vec<_> = (0..self.header.len())
            .map(|column| {
                if column < self.numeric_from {
                    "---"
                } else {
                    "---:"
                }
            })
            .collect();
        let _ = writeln!(output, "| {} |", rule.join(" | "));
        for row in &self.rows {
            let cells: Vec<_> = row.iter().map(|cell| cell.replace('|', "\\|")).collect();
            let _ = writeln!(output, "| {} |", cells.join(" | "));
        }
    }

    fn render(&self, output: &mut String) {
        let mut widths: Vec<_> = self.header.iter().map(|cell| cell.width()).collect();
        for row in &self.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width());
            }
        }
        let mut line = |cells: &[&str]| {
            let mut line = String::new();
            for (column, (cell, width)) in cells.iter().zip(&widths).enumerate() {
                let pad = " ".repeat(width - cell.width());
                let (before, after) = if column < self.numeric_from {
                    ("", pad.as_str())
                } else {
                    (pad.as_str(), "")
                };
                let gap = if column > 0 { "  " } else { "" };
                let _ = write!(line, "{gap}{before}{cell}{after}");
            }
            let _ = writeln!(output, "{}", line.trim_end());
        };
        line(self.header);
        let rule: Vec<_> = widths.iter().map(|width| "─".repeat(*width)).collect();
        line(&rule.iter().map(String::as_str).collect::<Vec<_>>());
        for row in &self.rows {
            line(&row.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
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
        duration(stats.started, stats.finished)
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
    let _ = writeln!(
        output,
        "{bullet}Tokens: {} input, {} cached, {} output · Tool calls: {} ({} errors)",
        number(totals.usage.input_tokens),
        number(totals.usage.cached_input_tokens),
        number(totals.usage.output_tokens),
        number(totals.tool_calls.calls),
        number(totals.tool_calls.errors),
    );
    let mut agents = Table {
        header: &[
            "Path", "Model", "Calls", "Tools", "Input", "Cached", "Output", "Duration",
        ],
        numeric_from: 2,
        rows: stats
            .agents
            .iter()
            .map(|agent| {
                vec![
                    diagnostic_text(&agent.path),
                    diagnostic_text(agent.model.as_deref().unwrap_or("—")),
                    ratio(&agent.requests),
                    number(tool_calls(agent)),
                    number(agent.usage.input_tokens),
                    number(agent.usage.cached_input_tokens),
                    number(agent.usage.output_tokens),
                    elapsed(agent),
                ]
            })
            .collect(),
    };
    agents.rows.push(vec![
        format!(
            "{} ({} agents)",
            if markdown { "**Total**" } else { "Total" },
            totals.agents
        ),
        String::new(),
        ratio(&stats.totals.requests),
        number(totals.tool_calls.calls),
        number(totals.usage.input_tokens),
        number(totals.usage.cached_input_tokens),
        number(totals.usage.output_tokens),
        duration(stats.started, stats.finished),
    ]);
    let models = Table {
        header: &["Model", "Calls", "Input", "Cached", "Output"],
        numeric_from: 1,
        rows: stats
            .models
            .iter()
            .map(|(name, model)| {
                vec![
                    diagnostic_text(name),
                    ratio(&model.requests),
                    number(model.usage.input_tokens),
                    number(model.usage.cached_input_tokens),
                    number(model.usage.output_tokens),
                ]
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
                    diagnostic_text(name),
                    number(tool.calls),
                    number(tool.errors),
                    number(tool.unanswered),
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
            table.render(&mut output);
        }
    }
    output
}

fn describe(agent: &AgentStats) -> String {
    let model = agent.model.as_deref().map_or_else(String::new, |model| {
        format!(" [{}]", diagnostic_text(model))
    });
    format!(
        "{}{} · {} calls · {} tools · in {} · cached {} · out {} · {}",
        if agent.depth == 0 { "/" } else { &agent.name },
        model,
        ratio(&agent.requests),
        number(tool_calls(agent)),
        number(agent.usage.input_tokens),
        number(agent.usage.cached_input_tokens),
        number(agent.usage.output_tokens),
        elapsed(agent),
    )
}

fn listing(sessions: &[SessionStats]) -> String {
    let table = Table {
        header: &[
            "Session", "Started", "Prompt", "Agents", "Calls", "Tools", "Input", "Cached",
            "Output", "Duration",
        ],
        numeric_from: 3,
        rows: sessions
            .iter()
            .map(|stats| {
                vec![
                    stats.session.to_string(),
                    timestamp(stats.started),
                    stats
                        .initial_prompt
                        .as_deref()
                        .map_or_else(|| "—".into(), |prompt| one_line(prompt, 60)),
                    number(stats.totals.agents),
                    ratio(&stats.totals.requests),
                    number(stats.totals.tool_calls.calls),
                    number(stats.totals.usage.input_tokens),
                    number(stats.totals.usage.cached_input_tokens),
                    number(stats.totals.usage.output_tokens),
                    duration(stats.started, stats.finished),
                ]
            })
            .collect(),
    };
    let mut output = String::new();
    table.render(&mut output);
    output
}

/// Agents come in depth-first order, so an agent is the last of its siblings when
/// no later agent at its depth precedes one shallower than it.
fn tree(stats: &SessionStats) -> String {
    let mut output = stats.session.to_string();
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
    let _ = writeln!(
        output,
        "\n{} agents, {} calls, {} tool calls, in {}, cached {}, out {}, {}",
        totals.agents,
        ratio(&totals.requests),
        number(totals.tool_calls.calls),
        number(totals.usage.input_tokens),
        number(totals.usage.cached_input_tokens),
        number(totals.usage.output_tokens),
        duration(stats.started, stats.finished),
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
            outcome: skyhook::session::stats::AgentOutcome::Completed,
            started: Default::default(),
            finished: None,
            usage: Default::default(),
            requests: Default::default(),
            compactions: Default::default(),
            tools: Default::default(),
            jobs: Default::default(),
            children: 0,
        };
        let stats = SessionStats {
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

    #[test]
    fn numbers_and_durations_format_for_people() {
        assert_eq!(number(0), "0");
        assert_eq!(number(999), "999");
        assert_eq!(number(1000), "1,000");
        assert_eq!(number(10_816_684), "10,816,684");
        let at = |millis| DateTime::from_timestamp_millis(millis).unwrap();
        assert_eq!(duration(at(0), at(5_000)), "5s");
        assert_eq!(duration(at(0), at(65_000)), "1m 05s");
        assert_eq!(duration(at(0), at(3_725_000)), "1h 02m 05s");
    }
}
