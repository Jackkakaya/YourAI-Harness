//! Local slash commands; completion never submits a model message.
#[derive(Clone, Copy)]
pub struct Command {
    pub text: &'static str,
    pub description: &'static str,
    pub argument: bool,
}
const COMMANDS: &[Command] = &[
    Command {
        text: "/new",
        description: "Start a fresh session",
        argument: false,
    },
    Command {
        text: "/yolo",
        description: "Toggle permissions (or /yolo on|off) · Ctrl-G",
        argument: false,
    },
    Command {
        text: "/help",
        description: "Show keyboard shortcuts",
        argument: false,
    },
    Command {
        text: "/compact",
        description: "Compact current context",
        argument: false,
    },
    Command {
        text: "/continue",
        description: "Resume pending inputs",
        argument: false,
    },
    Command {
        text: "/clear",
        description: "Reset context in a new session; keep saved history",
        argument: false,
    },
    Command {
        text: "/queue",
        description: "Schedule a follow-up turn",
        argument: true,
    },
    Command {
        text: "/theme",
        description: "Theme picker (or /theme NAME)",
        argument: false,
    },
    Command {
        text: "/models",
        description: "Switch model (picker; or /models p/m [variant])",
        argument: false,
    },
    Command {
        text: "/sessions",
        description: "List and switch sessions",
        argument: false,
    },
    Command {
        text: "/status",
        description: "Toggle stats dashboard",
        argument: false,
    },
    Command {
        text: "/quit",
        description: "Quit",
        argument: false,
    },
];
const THEMES: &[Command] = &[
    Command {
        text: "/theme system",
        description: "Follow OS",
        argument: false,
    },
    Command {
        text: "/theme dark",
        description: "Dark",
        argument: false,
    },
    Command {
        text: "/theme light",
        description: "Light",
        argument: false,
    },
    Command {
        text: "/theme one-dark",
        description: "Atom One Dark",
        argument: false,
    },
    Command {
        text: "/theme monokai",
        description: "Monokai",
        argument: false,
    },
    Command {
        text: "/theme solarized-dark",
        description: "Solarized Dark",
        argument: false,
    },
    Command {
        text: "/theme solarized-light",
        description: "Solarized Light",
        argument: false,
    },
    Command {
        text: "/theme nord",
        description: "Cool tones",
        argument: false,
    },
    Command {
        text: "/theme dracula",
        description: "Purple",
        argument: false,
    },
    Command {
        text: "/theme catppuccin",
        description: "Catppuccin Mocha",
        argument: false,
    },
    Command {
        text: "/theme tokyo-night",
        description: "Tokyo Night",
        argument: false,
    },
    Command {
        text: "/theme gruvbox",
        description: "Gruvbox",
        argument: false,
    },
];
#[derive(Default)]
pub struct Menu {
    query: String,
    pub selected: usize,
    dismissed: bool,
    enabled: bool,
}
impl Menu {
    pub fn sync(&mut self, text: &str, enabled: bool) {
        if self.query != text {
            self.query = text.into();
            self.selected = 0;
            self.dismissed = false;
        }
        self.enabled = enabled;
    }
    pub fn items(&self) -> Vec<Command> {
        if !self.enabled
            || self.dismissed
            || !self.query.starts_with('/')
            || self.query.contains('\n')
        {
            return vec![];
        }
        let source = if self.query.starts_with("/theme ") {
            THEMES
        } else {
            COMMANDS
        };
        source
            .iter()
            .copied()
            .filter(|c| c.text.starts_with(&self.query))
            .collect()
    }
    pub fn step(&mut self, backwards: bool) {
        let count = self.items().len();
        if count > 0 {
            self.selected = if backwards {
                (self.selected + count - 1) % count
            } else {
                (self.selected + 1) % count
            };
        }
    }
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filter_navigation_dismissal_and_arguments() {
        let mut menu = Menu::default();
        menu.sync("/", true);
        assert_eq!(menu.items().len(), 12);
        menu.step(true);
        assert_eq!(menu.items()[menu.selected].text, "/quit");
        menu.sync("/co", true);
        assert_eq!(menu.items().len(), 2);
        menu.dismiss();
        assert!(menu.items().is_empty());
        menu.sync("/theme ", true);
        assert_eq!(menu.items().len(), 12);
        menu.sync("/queue work", true);
        assert!(menu.items().is_empty());
        menu.sync("/", false);
        assert!(menu.items().is_empty());
    }
}
