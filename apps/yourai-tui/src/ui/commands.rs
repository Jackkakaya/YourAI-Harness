//! Local slash commands: menu rows, parsing and completion. `Parsed` is the
//! single vocabulary between the menu, `parse` and the dispatch match in
//! ui.rs — adding a command means one variant here, one COMMANDS row and one
//! dispatch arm; the exhaustive match keeps them from drifting apart.

/// A parsed slash command. Dispatch (and its side effects) lives in ui.rs;
/// this type only carries what the command means.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    /// `/new` / `/clear`: fresh context in a new session.
    New,
    /// `/yolo`, `/yolo on|off`.
    Yolo(YoloArg),
    Help,
    Compact,
    Continue,
    Status,
    Quit,
    /// `/queue TEXT`: schedule a follow-up turn.
    Queue(String),
    /// `/theme` (None) opens the picker; a name switches directly.
    Theme(Option<String>),
    /// `/models` (no id) opens the picker; `/models p/m [variant]` switches.
    Models {
        id: Option<String>,
        variant: Option<String>,
    },
    Sessions,
}

#[derive(Debug, PartialEq)]
pub enum YoloArg {
    Toggle,
    On,
    Off,
    /// Anything else: the dispatcher reports the usage line.
    Usage,
}

/// Parse a raw input line into a command. `None` means "not a command":
/// either plain user text, or an unknown `/word` (the caller decides which
/// by re-checking the raw leading slash).
///
/// `/queue` keeps its historical raw-prefix semantics: only a line that
/// starts with `"/queue "` exactly (no leading whitespace) queues a
/// follow-up — a spaced-out variant stays plain user text.
pub fn parse(text: &str) -> Option<Parsed> {
    if let Some(body) = text.strip_prefix("/queue ") {
        return Some(Parsed::Queue(body.to_string()));
    }
    let trimmed = text.trim();
    let (head, rest) = match trimmed.split_once(' ') {
        Some((head, rest)) => (head, Some(rest.trim())),
        None => (trimmed, None),
    };
    let rest = move || rest.map(str::to_string);
    match head {
        "/quit" => Some(Parsed::Quit),
        "/help" => Some(Parsed::Help),
        "/new" | "/clear" => Some(Parsed::New),
        "/continue" => Some(Parsed::Continue),
        "/status" => Some(Parsed::Status),
        "/sessions" => Some(Parsed::Sessions),
        "/compact" => Some(Parsed::Compact),
        "/yolo" => Some(Parsed::Yolo(match rest().as_deref() {
            None => YoloArg::Toggle,
            Some("on") => YoloArg::On,
            Some("off") => YoloArg::Off,
            Some(_) => YoloArg::Usage,
        })),
        "/theme" => Some(Parsed::Theme(rest())),
        "/models" => {
            // No argument opens the picker; the direct form switches.
            match rest() {
                None => Some(Parsed::Models {
                    id: None,
                    variant: None,
                }),
                Some(arg) => {
                    let mut parts = arg.splitn(2, ' ');
                    Some(Parsed::Models {
                        id: parts.next().map(str::to_string),
                        variant: parts.next().map(str::trim).map(str::to_string),
                    })
                }
            }
        }
        _ => None,
    }
}

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
    #[allow(clippy::wildcard_imports)]
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

    #[test]
    fn menu_rows_and_parse_agree() {
        // Every menu row must parse to a real command; argument rows need
        // an argument to be meaningful. This is the guard against adding a
        // COMMANDS entry without a dispatch arm (and vice versa).
        for command in COMMANDS {
            if command.argument {
                let text = format!("{} example", command.text);
                assert!(parse(&text).is_some(), "argument row: {text}");
            } else {
                assert!(parse(command.text).is_some(), "row: {}", command.text);
            }
        }
        for theme in THEMES {
            assert!(
                matches!(parse(theme.text), Some(Parsed::Theme(Some(_)))),
                "theme row: {}",
                theme.text
            );
        }
    }

    #[test]
    fn parse_covers_arg_shapes_and_rejects_unknowns() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("/unknown"), None);
        assert_eq!(parse("/queue"), None, "bare /queue is not a command");
        assert_eq!(parse("/queue "), Some(Parsed::Queue(String::new())));
        // Raw-prefix semantics: leading whitespace turns /queue into plain
        // user text (the historical behavior), not a follow-up.
        assert_eq!(parse("  /queue hi  "), None);
        assert_eq!(parse("/queue hi"), Some(Parsed::Queue("hi".into())));
        assert_eq!(parse("/yolo"), Some(Parsed::Yolo(YoloArg::Toggle)));
        assert_eq!(parse("/yolo on"), Some(Parsed::Yolo(YoloArg::On)));
        assert_eq!(parse("/yolo junk"), Some(Parsed::Yolo(YoloArg::Usage)));
        assert_eq!(parse("/theme"), Some(Parsed::Theme(None)));
        assert_eq!(
            parse("/theme nord"),
            Some(Parsed::Theme(Some("nord".into())))
        );
        assert_eq!(
            parse("/models"),
            Some(Parsed::Models {
                id: None,
                variant: None
            })
        );
        assert_eq!(
            parse("/models gateway/kimi-k3 short"),
            Some(Parsed::Models {
                id: Some("gateway/kimi-k3".into()),
                variant: Some("short".into())
            })
        );
        assert_eq!(
            parse("/models gateway/kimi-k3"),
            Some(Parsed::Models {
                id: Some("gateway/kimi-k3".into()),
                variant: None
            })
        );
    }
}
