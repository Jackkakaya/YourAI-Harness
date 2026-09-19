//! Local slash commands; completion never submits a model message.
#[derive(Clone, Copy)]
pub struct Command {
    pub text: &'static str,
    pub description: &'static str,
    pub argument: bool,
}
const COMMANDS: &[Command] = &[
    Command {
        text: "/help",
        description: "查看快捷键",
        argument: false,
    },
    Command {
        text: "/compact",
        description: "压缩当前上下文",
        argument: false,
    },
    Command {
        text: "/continue",
        description: "继续处理待执行输入",
        argument: false,
    },
    Command {
        text: "/clear",
        description: "清屏，保留会话历史",
        argument: false,
    },
    Command {
        text: "/queue",
        description: "添加后续任务",
        argument: true,
    },
    Command {
        text: "/theme",
        description: "选择颜色主题",
        argument: true,
    },
    Command {
        text: "/quit",
        description: "退出",
        argument: false,
    },
];
const THEMES: &[Command] = &[
    Command {
        text: "/theme dark",
        description: "深色",
        argument: false,
    },
    Command {
        text: "/theme light",
        description: "浅色",
        argument: false,
    },
    Command {
        text: "/theme nord",
        description: "冷色",
        argument: false,
    },
    Command {
        text: "/theme dracula",
        description: "紫色",
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
        assert_eq!(menu.items().len(), 7);
        menu.step(true);
        assert_eq!(menu.items()[menu.selected].text, "/quit");
        menu.sync("/co", true);
        assert_eq!(menu.items().len(), 2);
        menu.dismiss();
        assert!(menu.items().is_empty());
        menu.sync("/theme ", true);
        assert_eq!(menu.items().len(), 4);
        menu.sync("/queue work", true);
        assert!(menu.items().is_empty());
        menu.sync("/", false);
        assert!(menu.items().is_empty());
    }
}
