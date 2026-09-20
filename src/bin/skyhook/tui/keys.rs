use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use std::{fmt, str::FromStr};

/// One source of truth for command names, discoverability, and default shortcuts.
pub struct CommandSpec {
    pub command: Command,
    pub id: &'static str,
    pub label: &'static str,
    pub default_binding: &'static str,
    pub aliases: &'static [&'static str],
    pub palette: bool,
}

// Generate the enum and registry together so their order and membership cannot drift.
macro_rules! commands {
    ($($variant:ident => ($id:literal, $label:literal, $binding:literal, $aliases:expr, $palette:literal)),* $(,)?) => {
        /// Actions understood by the TUI, independent of their invocation source.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Command { $($variant),* }

        pub const COMMANDS: &[CommandSpec] = &[$(CommandSpec {
            command: Command::$variant, id: $id, label: $label,
            default_binding: $binding, aliases: $aliases,
            palette: $palette,
        }),*];

        impl Command {
            pub const fn spec(self) -> &'static CommandSpec {
                &COMMANDS[self as usize]
            }

            pub const fn id(self) -> &'static str {
                self.spec().id
            }
        }
    };
}

commands! {
    // Canonical ID, label, default shortcut, aliases, palette visibility.
    New => ("new", "New session", "ctrl+x n", &[], true),
    Sessions => ("sessions", "Switch session", "ctrl+x s", &["switch"], true),
    Close => ("close", "Close session", "ctrl+x w", &[], true),
    Model => ("model", "Model", "ctrl+x m", &["models"], true),
    Mode => ("mode", "Mode", "ctrl+x p", &["modes"], true),
    Agents => ("agents", "Inspect agent", "ctrl+x a", &[], true),
    Inspect => ("inspect", "Focus conversation", "", &[], false),
    Copy => ("copy", "Copy message", "ctrl+x y", &[], true),
    Export => ("export", "Export conversation", "ctrl+x x", &[], true),
    Exit => ("exit", "Quit", "ctrl+x q", &[], true),
    Child => ("child", "First child", "", &[], false),
    Parent => ("parent", "Parent agent", "", &[], false),
    PreviousAgent => ("previous-agent", "Agent above", "ctrl+x up", &[], false),
    NextAgent => ("next-agent", "Agent below", "ctrl+x down", &[], false),
    PreviousTab => ("previous-tab", "Previous tab", "ctrl+x left", &[], false),
    NextTab => ("next-tab", "Next tab", "ctrl+x right", &[], false),
    Commands => ("commands", "Command palette", "ctrl+p", &[], false),
    Details => ("details", "Toggle tool details", "ctrl+x t", &[], true),
    Attachments => ("attachments", "Inspect or remove attachments", "", &[], true),
    Queue => ("queue", "Edit queued follow-ups", "ctrl+x i", &[], true),
    Resume => ("resume", "Resume queued input", "", &[], true),
    Retry => ("retry", "Continue failed or interrupted turns", "ctrl+x c", &["continue"], true),
    Attention => ("attention", "Reopen questions and permissions", "ctrl+x r", &[], true),
    Help => ("help", "Help and shortcuts", "ctrl+x h", &[], true),
    Files => ("files", "Attach workspace file", "ctrl+x f", &[], true),
    Sidebar => ("sidebar", "Toggle sidebar", "ctrl+x b", &[], true),
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for Command {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        COMMANDS
            .iter()
            .find(|spec| spec.id == value || spec.aliases.contains(&value))
            .map(|spec| spec.command)
            .ok_or_else(|| format!("Unknown command: {value}"))
    }
}

/// A single key or a two-key chord as `(prefix, key)`.
type KeySequence = (Option<KeyEvent>, KeyEvent);

fn parse_sequence(value: &str) -> KeySequence {
    let keys = value.split_whitespace().map(parse).collect::<Vec<_>>();
    match keys[..] {
        [key] => (None, key),
        [prefix, key] => (Some(prefix), key),
        _ => panic!("Invalid keybinding: {value}"),
    }
}

fn display_sequence((prefix, key): KeySequence) -> String {
    prefix
        .iter()
        .chain([&key])
        .map(display)
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct KeyMap {
    bindings: Vec<(KeySequence, Command)>,
}
impl Default for KeyMap {
    fn default() -> Self {
        let bindings = COMMANDS
            .iter()
            .filter(|spec| !spec.default_binding.is_empty())
            .map(|spec| (parse_sequence(spec.default_binding), spec.command))
            .collect();
        Self { bindings }
    }
}
impl KeyMap {
    pub fn action(&self, prefix: Option<KeyEvent>, key: KeyEvent) -> Option<Command> {
        let sequence = (prefix, key);
        self.bindings
            .iter()
            .find(|(keys, _)| *keys == sequence)
            .map(|(_, command)| *command)
    }
    pub fn prefix(&self, key: KeyEvent) -> bool {
        self.bindings
            .iter()
            .any(|(keys, _)| matches!(*keys, (Some(prefix), _) if prefix == key))
    }
    pub fn leader_hint(&self, prefix: KeyEvent, visible: &[(Command, &str)]) -> String {
        let hints = visible
            .iter()
            .filter_map(|(action, label)| {
                self.bindings.iter().find_map(|(keys, command)| match keys {
                    (Some(leader), key) if *leader == prefix && command == action => {
                        Some(format!("{} {label}", display(key)))
                    }
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join(" · ");
        format!("{}: {hints}", display(&prefix))
    }
    pub fn binding(&self, action: Command) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, command)| *command == action)
            .map(|(keys, _)| display_sequence(*keys))
    }
    pub fn help(&self) -> String {
        COMMANDS
            .iter()
            .map(|spec| {
                let keys = self.binding(spec.command).unwrap_or_default();
                let id = spec.id;
                let label = spec.label;
                format!("{keys:20} /{id:12} {label}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
/// Bindings are built-in, so an invalid one is a programming error.
fn parse(text: &str) -> KeyEvent {
    let text = text.to_ascii_lowercase();
    let mut modifiers = M::NONE;
    let mut code = None;
    for part in text.split('+') {
        match part {
            "ctrl" => modifiers |= M::CONTROL,
            "alt" => modifiers |= M::ALT,
            "shift" => modifiers |= M::SHIFT,
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "enter" => code = Some(KeyCode::Enter),
            "tab" => code = Some(KeyCode::Tab),
            s if s.chars().count() == 1 => code = s.chars().next().map(KeyCode::Char),
            _ => panic!("Invalid keybinding: {text}"),
        }
    }
    KeyEvent::new(code.expect("keybinding names a key"), modifiers)
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

    use super::parse as key;

    #[test]
    fn parsing_requires_exact_command_names() {
        assert!("unknown".parse::<Command>().is_err());
        assert!("/resume".parse::<Command>().is_err());
        assert!("resume later".parse::<Command>().is_err());
        assert_eq!("resume".parse::<Command>().unwrap(), Command::Resume);
        assert_eq!("models".parse::<Command>().unwrap().to_string(), "model");
        assert_eq!("retry".parse::<Command>().unwrap(), Command::Retry);
        assert_eq!("continue".parse::<Command>().unwrap(), Command::Retry);
        assert_eq!("continue".parse::<Command>().unwrap().to_string(), "retry");
    }

    #[test]
    fn default_bindings_resolve_to_typed_commands_and_unbound_ones_are_not_advertised() {
        let keys = KeyMap::default();
        let leader = key("ctrl+x");
        assert_eq!(keys.action(Some(leader), key("m")), Some(Command::Model));
        assert_eq!(keys.action(Some(leader), key("f")), Some(Command::Files));
        assert_eq!(keys.binding(Command::Files).as_deref(), Some("Ctrl+X F"));
        assert_eq!(keys.action(Some(leader), key("c")), Some(Command::Retry));
        assert_eq!(keys.binding(Command::Retry).as_deref(), Some("Ctrl+X C"));
        assert_eq!(
            keys.binding(Command::Retry),
            Some(display_sequence(parse_sequence("ctrl+x c")))
        );
        assert_eq!(keys.action(None, key("ctrl+p")), Some(Command::Commands));
        assert!(keys.prefix(leader));
        assert_eq!(keys.binding(Command::Attachments), None);
        let hint = keys.leader_hint(
            leader,
            &[
                (Command::Attachments, "Attachments"),
                (Command::Model, "Model"),
            ],
        );
        assert_eq!(hint, "Ctrl+X: M Model");
    }

    #[test]
    fn default_bindings_do_not_conflict() {
        let bindings = &KeyMap::default().bindings;
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
