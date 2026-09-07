use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use std::collections::BTreeMap;

pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("new", "New session", "ctrl+x n"),
    ("sessions", "Resume session", "ctrl+x l"),
    ("model", "Model", "ctrl+x m"),
    ("agents", "Inspect agent", "ctrl+x a"),
    ("inspect", "Focus conversation", "ctrl+x i"),
    ("state", "Agent state", "ctrl+x s"),
    ("themes", "Theme", "ctrl+x t"),
    ("editor", "External editor", "ctrl+x e"),
    ("copy", "Copy message", "ctrl+x y"),
    ("export", "Export conversation", "ctrl+x x"),
    ("exit", "Quit", "ctrl+x q"),
    ("child", "First child", "ctrl+x down"),
    ("parent", "Parent agent", "ctrl+x up"),
    ("commands", "Command palette", "ctrl+p"),
    ("profiles", "Instruction profile for new sessions", ""),
    ("jobs", "Agent jobs", ""),
    ("requests", "Model requests", ""),
    ("thinking", "Expand/collapse saved reasoning", ""),
    ("details", "Toggle tool details", ""),
    ("attach", "Attach an image", ""),
    ("attachments", "Inspect or remove attachments", ""),
    ("queue", "Edit queued follow-ups", ""),
    ("resume", "Resume queued input", ""),
    ("retry", "Continue after a failed or interrupted turn", ""),
    ("attention", "Pending questions and permissions", ""),
    ("diagnostics", "Startup diagnostics", ""),
    ("help", "Help and shortcuts", ""),
];

pub struct KeyMap {
    bindings: Vec<(Vec<KeyEvent>, String)>,
}
impl KeyMap {
    pub fn new(overrides: &BTreeMap<String, String>) -> Result<Self, String> {
        for name in overrides.keys() {
            if name != "models" && !COMMANDS.iter().any(|(id, _, _)| id == name) {
                return Err(format!("Unknown keybinding action: {name}"));
            }
        }
        let mut bindings = Vec::new();
        for (id, _, default) in COMMANDS {
            let value = overrides
                .get(*id)
                .or_else(|| (*id == "model").then(|| overrides.get("models")).flatten())
                .map(String::as_str)
                .unwrap_or(default);
            if value.is_empty() || value == "none" {
                continue;
            }
            let sequence = value
                .split_whitespace()
                .map(parse)
                .collect::<Result<Vec<_>, _>>()?;
            if sequence.len() > 2 {
                return Err(format!("Keybinding too long: {value}"));
            }
            if bindings.iter().any(|(other, _): &(Vec<KeyEvent>, String)| {
                other.starts_with(&sequence) || sequence.starts_with(other)
            }) {
                return Err(format!("Conflicting keybinding: {value}"));
            }
            bindings.push((sequence, (*id).to_owned()));
        }
        Ok(Self { bindings })
    }
    pub fn action(&self, prefix: Option<KeyEvent>, key: KeyEvent) -> Option<String> {
        let sequence = prefix.map_or_else(|| vec![key], |p| vec![p, key]);
        self.bindings
            .iter()
            .find(|(keys, _)| *keys == sequence)
            .map(|(_, id)| id.clone())
    }
    pub fn prefix(&self, key: KeyEvent) -> bool {
        self.bindings
            .iter()
            .any(|(keys, _)| keys.len() == 2 && keys[0] == key)
    }
    pub fn binding(&self, action: &str) -> String {
        self.bindings
            .iter()
            .find(|(_, id)| id == action)
            .map(|(keys, _)| keys.iter().map(display).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }
    pub fn help(&self) -> String {
        COMMANDS
            .iter()
            .map(|(id, label, _)| {
                let keys = self.binding(id);
                format!("{keys:20} /{id:12} {label}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
fn parse(text: &str) -> Result<KeyEvent, String> {
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
            _ => return Err(format!("Invalid keybinding: {text}")),
        }
    }
    code.map(|code| KeyEvent::new(code, modifiers))
        .ok_or_else(|| format!("Invalid keybinding: {text}"))
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
    fn model_shortcut_accepts_legacy_alias_and_prefers_canonical_override() {
        let legacy = BTreeMap::from([("models".into(), "ctrl+x z".into())]);
        let keys = KeyMap::new(&legacy).unwrap();
        assert_eq!(
            keys.action(Some(parse("ctrl+x").unwrap()), parse("z").unwrap())
                .as_deref(),
            Some("model")
        );
        let mut overrides = legacy;
        overrides.insert("model".into(), "alt+m".into());
        let keys = KeyMap::new(&overrides).unwrap();
        assert_eq!(
            keys.action(None, parse("alt+m").unwrap()).as_deref(),
            Some("model")
        );
        assert!(
            keys.action(Some(parse("ctrl+x").unwrap()), parse("z").unwrap())
                .is_none()
        );
        assert!(keys.help().contains("/model"));
        assert!(!keys.help().contains("Model for new sessions"));
    }
}
