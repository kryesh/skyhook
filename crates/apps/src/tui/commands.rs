use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use std::{fmt, str::FromStr};

/// One source of truth for command names, discoverability, and default shortcuts.
pub struct CommandSpec {
    pub command: Command,
    /// What it is typed by, as `/id`; none for a command only its keys run.
    pub id: Option<&'static str>,
    pub label: &'static str,
    pub binding: Option<KeySequence>,
    /// Keys that also run it while the content pane has focus, whatever their modifiers.
    pub context: &'static [(Context, KeyCode)],
    pub aliases: &'static [&'static str],
    pub palette: bool,
}

/// What the focused content pane shows for a context key to run its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// Anything: an inspector tab or a page.
    Content,
    /// An inspector tab, whose rows the command acts on.
    Inspector,
}

// Generate the enum and registry together so their order and membership cannot drift.
macro_rules! commands {
    (@id _) => { None };
    (@id $id:literal) => { Some($id) };
    ($($variant:ident => ($id:tt, $label:literal, $binding:expr, $aliases:expr, $palette:literal $(, $context:ident: $code:expr)?)),* $(,)?) => {
        /// Actions understood by the TUI, independent of their invocation source.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Command { $($variant),* }

        pub const COMMANDS: &[CommandSpec] = &[$(CommandSpec {
            command: Command::$variant, id: commands!(@id $id), label: $label,
            binding: $binding, context: &[$((Context::$context, $code))?], aliases: $aliases,
            palette: $palette,
        }),*];

        impl Command {
            pub const fn spec(self) -> &'static CommandSpec {
                &COMMANDS[self as usize]
            }
        }
    };
}

commands! {
    // Canonical ID (`_`: keys alone), label, default shortcut, aliases, palette, context key.
    New => ("new", "New session", chord(KeyCode::Char('n')), &[], true),
    Sessions => ("sessions", "Switch session", chord(KeyCode::Char('s')), &["switch"], true),
    Close => ("close", "Close session", chord(KeyCode::Char('w')), &[], true),
    Rename => ("rename", "Rename session", chord(KeyCode::Char('l')), &[], true),
    Model => ("model", "Model", chord(KeyCode::Char('m')), &["models"], true),
    Mode => ("mode", "Mode", chord(KeyCode::Char('p')), &["modes"], true),
    Agents => ("agents", "Inspect agent", chord(KeyCode::Char('a')), &[], true),
    Inspect => ("inspect", "Focus conversation", None, &[], false),
    Copy => ("copy", "Copy message", chord(KeyCode::Char('y')), &[], true, Inspector: KeyCode::Char('y')),
    Export => ("export", "Export conversation", chord(KeyCode::Char('x')), &[], true),
    Exit => ("exit", "Quit", chord(KeyCode::Char('q')), &[], true),
    Child => ("child", "First child", None, &[], false),
    Parent => ("parent", "Parent agent", None, &[], false),
    PreviousAgent => ("previous-agent", "Agent above", chord(KeyCode::Up), &[], false),
    NextAgent => ("next-agent", "Agent below", chord(KeyCode::Down), &[], false),
    PreviousTab => ("previous-tab", "Previous tab", chord(KeyCode::Left), &[], false, Content: KeyCode::Char('[')),
    NextTab => ("next-tab", "Next tab", chord(KeyCode::Right), &[], false, Content: KeyCode::Char(']')),
    HalfPageUp => (_, "Scroll up half a page", ctrl_alt(KeyCode::Char('u')), &[], false),
    HalfPageDown => (_, "Scroll down half a page", ctrl_alt(KeyCode::Char('d')), &[], false),
    Top => (_, "Scroll to the beginning", None, &[], false, Content: KeyCode::Home),
    Bottom => (_, "Scroll to the latest", None, &[], false, Content: KeyCode::End),
    Commands => ("commands", "Command palette", ctrl(KeyCode::Char('p')), &[], false),
    Details => ("details", "Toggle tool details", chord(KeyCode::Char('t')), &[], true),
    CancelJob => (_, "Cancel the selected job", None, &[], false, Inspector: KeyCode::Char('c')),
    Attachments => ("attachments", "Inspect or remove attachments", None, &[], true),
    Queue => ("queue", "Edit queued follow-ups", chord(KeyCode::Char('i')), &[], true),
    Resume => ("resume", "Resume queued input", None, &[], true),
    Retry => ("retry", "Continue failed or interrupted turns", chord(KeyCode::Char('c')), &["continue"], true),
    Attention => ("attention", "Reopen questions and permissions", chord(KeyCode::Char('r')), &[], true),
    Help => ("help", "Help and shortcuts", chord(KeyCode::Char('h')), &[], true),
    Files => ("files", "Attach workspace file", chord(KeyCode::Char('f')), &[], true),
    Sidebar => ("sidebar", "Toggle sidebar", chord(KeyCode::Char('b')), &[], true),
    Scope => ("scope", "Edit scope", chord(KeyCode::Char('o')), &[], true),
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.spec().id.map_or(String::new(), |id| format!("/{id}")))
    }
}

impl FromStr for Command {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        COMMANDS
            .iter()
            .find(|spec| spec.id == Some(value) || spec.aliases.contains(&value))
            .map(|spec| spec.command)
            .ok_or_else(|| format!("Unknown command: {value}"))
    }
}

/// A single key or a two-key chord as `(prefix, key)`.
pub type KeySequence = (Option<KeyEvent>, KeyEvent);

/// The prefix of every chord.
const LEADER: KeyEvent = KeyEvent::new(KeyCode::Char('x'), M::CONTROL);

/// The leader, then `code`.
const fn chord(code: KeyCode) -> Option<KeySequence> {
    Some((Some(LEADER), KeyEvent::new(code, M::NONE)))
}

/// `code` with Ctrl held.
const fn ctrl(code: KeyCode) -> Option<KeySequence> {
    Some((None, KeyEvent::new(code, M::CONTROL)))
}

/// `code` with Ctrl and Alt held.
const fn ctrl_alt(code: KeyCode) -> Option<KeySequence> {
    Some((None, KeyEvent::new(code, M::CONTROL.union(M::ALT))))
}

fn display_sequence((prefix, key): KeySequence) -> String {
    prefix
        .iter()
        .chain([&key])
        .map(display)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every command's binding, as the registry gives it.
fn bindings() -> impl Iterator<Item = (KeySequence, Command)> {
    COMMANDS
        .iter()
        .filter_map(|spec| Some((spec.binding?, spec.command)))
}

/// The command `key` runs after `prefix`, if a binding names it.
pub fn action(prefix: Option<KeyEvent>, key: KeyEvent) -> Option<Command> {
    let sequence = (prefix, key);
    bindings().find_map(|(keys, command)| (keys == sequence).then_some(command))
}

/// The command `code` runs with the content pane focused and showing `shown`.
pub fn context_action(shown: Context, code: KeyCode) -> Option<Command> {
    let runs = |(context, key): &(Context, KeyCode)| {
        *key == code && [Context::Content, shown].contains(context)
    };
    let mut specs = COMMANDS.iter();
    specs.find_map(|spec| spec.context.iter().any(runs).then_some(spec.command))
}

/// Whether `key` begins a chord.
pub fn is_prefix(key: KeyEvent) -> bool {
    bindings().any(|(keys, _)| matches!(keys, (Some(prefix), _) if prefix == key))
}

pub fn leader_hint(prefix: KeyEvent, visible: &[(Command, &str)]) -> String {
    let hints = visible
        .iter()
        .filter_map(|(action, label)| {
            bindings().find_map(|(keys, command)| match keys {
                (Some(leader), key) if leader == prefix && command == *action => {
                    Some(format!("{} {label}", display(&key)))
                }
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{}: {hints}", display(&prefix))
}

impl Command {
    /// How its binding is written, if it has one.
    pub fn binding(self) -> Option<String> {
        self.spec().binding.map(display_sequence)
    }
}

/// Every typed command with its binding, then the keys under the focus they need.
pub fn help() -> String {
    let row = |keys, spec: &CommandSpec| format!("{keys:20} {:13} {}", spec.command, spec.label);
    let bound = |spec: &CommandSpec| row(spec.command.binding().unwrap_or_default(), spec);
    let (typed, keyed): (Vec<_>, Vec<_>) = COMMANDS.iter().partition(|spec| spec.id.is_some());
    let mut lines: Vec<_> = typed.into_iter().map(bound).collect();
    lines.extend([String::new(), "Any pane focused:".to_owned()]);
    let keyed = keyed.into_iter().filter(|spec| spec.binding.is_some());
    lines.extend(keyed.map(bound));
    for (context, heading) in [
        (Context::Content, "Content focused:"),
        (Context::Inspector, "Inspector tab focused:"),
    ] {
        lines.extend([String::new(), heading.to_owned()]);
        for spec in COMMANDS {
            let keys = spec.context.iter().filter(|(needed, _)| *needed == context);
            lines.extend(keys.map(|(_, code)| row(display(&KeyEvent::new(*code, M::NONE)), spec)));
        }
    }
    lines.join("\n")
}
fn display(key: &KeyEvent) -> String {
    let mut result = String::new();
    if key.modifiers.contains(M::CONTROL) {
        result.push_str("Ctrl+");
    }
    if key.modifiers.contains(M::ALT) {
        result.push_str("Alt+");
    }
    if key.modifiers.contains(M::SHIFT) {
        result.push_str("Shift+");
    }
    result.push_str(&match key.code {
        KeyCode::Char(c) => c.to_uppercase().to_string(),
        ref code => format!("{code:?}"),
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_exactly_and_unbound_ones_are_not_advertised() {
        for invalid in ["unknown", "/resume", "resume later"] {
            assert!(invalid.parse::<Command>().is_err(), "{invalid}");
        }
        assert!(is_prefix(LEADER));
        assert_eq!(Command::Attachments.binding(), None);
        let hint = leader_hint(
            LEADER,
            &[
                (Command::Attachments, "Attachments"),
                (Command::Model, "Model"),
            ],
        );
        assert_eq!(hint, "Ctrl+X: M Model");
    }

    /// A context key runs its command only where the pane shows what the command
    /// needs, and `/help` lists it under that focus.
    #[test]
    fn context_keys_follow_what_the_content_pane_shows() {
        for (shown, code, command) in [
            (
                Context::Inspector,
                KeyCode::Char('c'),
                Some(Command::CancelJob),
            ),
            (Context::Content, KeyCode::Char('c'), None),
            (Context::Inspector, KeyCode::End, Some(Command::Bottom)),
            (Context::Content, KeyCode::End, Some(Command::Bottom)),
        ] {
            assert_eq!(context_action(shown, code), command, "{shown:?} {code:?}");
        }
        let help = help();
        let (_, keys) = help.split_once("\n\nAny pane focused:\n").unwrap();
        let (any, keys) = keys.split_once("\n\nContent focused:\n").unwrap();
        let (content, inspector) = keys.split_once("\n\nInspector tab focused:\n").unwrap();
        // A row names its key, then how its command is typed, if it can be, then its label.
        let lists = |section: &str, key: &str, next: &str| {
            let mut rows = section.lines().map(|row| row.split_whitespace());
            rows.any(|mut row| row.next() == Some(key) && row.next() == Some(next))
        };
        assert!(lists(any, "Ctrl+Alt+U", "Scroll"));
        assert!(lists(content, "End", "Scroll") && !lists(content, "Y", "/copy"));
        assert!(lists(inspector, "Y", "/copy") && !lists(inspector, "End", "Scroll"));
    }

    #[test]
    fn default_bindings_do_not_conflict() {
        let bindings: Vec<_> = bindings().collect();
        for (index, &((a_prefix, a_key), _)) in bindings.iter().enumerate() {
            for &((b_prefix, b_key), _) in &bindings[index + 1..] {
                // Sequences conflict when either is a prefix of the other.
                let conflict = match (a_prefix, b_prefix) {
                    (Some(a), Some(b)) => a == b && a_key == b_key,
                    (Some(a), None) => a == b_key,
                    (None, Some(b)) => b == a_key,
                    (None, None) => a_key == b_key,
                };
                assert!(
                    !conflict,
                    "{} conflicts with {}",
                    display_sequence((a_prefix, a_key)),
                    display_sequence((b_prefix, b_key))
                );
            }
        }
    }
}
