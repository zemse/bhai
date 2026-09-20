//! The `/` menu: the harness commands and the session's skills, filtered by what has
//! been typed. This decides what the menu shows and what accepting a row puts in the
//! input; `App::submit` is what actually runs the command.

use crate::skills::Skill;

/// Rows the menu draws before it scrolls to keep the highlighted one in view.
pub const MAX_ROWS: usize = 8;

/// A harness command. `args` is the hint drawn after the name, empty when it takes none.
pub struct Command {
    pub name: &'static str,
    pub args: &'static str,
    pub help: &'static str,
}

/// Every command the prompt box accepts, in menu order.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "help",
        args: "",
        help: "the commands and the keys",
    },
    Command {
        name: "diff",
        args: "",
        help: "what has changed in the working tree",
    },
    // No args hint, though it takes `[prompt]`: compacting on its own is the usual way
    // in, and a hint would make enter fill the prompt rather than run it.
    Command {
        name: "compact",
        args: "",
        help: "summarise the conversation, /compact <prompt> to steer it",
    },
    Command {
        name: "clear",
        args: "",
        help: "drop the conversation and start over",
    },
    Command {
        name: "context",
        args: "",
        help: "write the context to .bhai/debug",
    },
    Command {
        name: "permissions",
        args: "",
        help: "the rules that decide approvals",
    },
    Command {
        name: "trust",
        args: "",
        help: "trust this project's settings files",
    },
    Command {
        name: "untrust",
        args: "",
        help: "stop trusting them",
    },
    Command {
        name: "skills",
        args: "",
        help: "the skills listed in the prompt",
    },
    Command {
        name: "workflows",
        args: "",
        help: "the workflow definitions found",
    },
    Command {
        name: "workflow",
        args: " <name> [input]",
        help: "hand a workflow to the agent",
    },
    Command {
        name: "queue",
        args: " [clear]",
        help: "prompts waiting behind the turn",
    },
    // No args hint, though it takes `<name> [effort]`: the picker is the usual way in,
    // and a hint would make enter fill the prompt rather than open it.
    Command {
        name: "model",
        args: "",
        help: "pick the model this session talks to",
    },
    Command {
        name: "mcp",
        args: "",
        help: "the MCP servers and their tools",
    },
    Command {
        name: "as",
        args: "",
        help: "the identity this session runs as",
    },
    Command {
        name: "tokens",
        args: "",
        help: "toggle the per-entry token badges",
    },
    Command {
        name: "copy",
        args: "",
        help: "copy the selection to the clipboard",
    },
    Command {
        name: "mouse",
        args: "",
        help: "toggle mouse capture",
    },
    Command {
        name: "quit",
        args: "",
        help: "leave bhai",
    },
];

/// One menu row.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub args: String,
    pub help: String,
    /// A skill row: accepting it writes a prompt for the agent, not a command.
    pub skill: bool,
}

impl Item {
    /// The text `/` plus the name, as it goes into the input.
    pub fn label(&self) -> String {
        format!("/{}", self.name)
    }

    /// What completing this row would add to what has been typed, for the grey tail
    /// drawn past the cursor. Empty once the name is there in full, and when the case
    /// differs, since tab would rewrite what is already on screen.
    pub fn completion(&self, value: &str) -> String {
        let Some(typed) = typing(value) else {
            return String::new();
        };
        self.name
            .strip_prefix(typed)
            .unwrap_or_default()
            .to_string()
    }

    /// Whether accepting the row should leave the input open for more typing.
    pub fn takes_input(&self) -> bool {
        self.skill || !self.args.is_empty()
    }
}

/// The name being typed, while the whole input is one `/word`. `None` once it holds a
/// space or a newline, so a prompt that merely mentions a path keeps the menu shut.
pub fn typing(value: &str) -> Option<&str> {
    let name = value.strip_prefix('/')?;
    let plain = !name.contains(|c: char| c.is_whitespace() || c == '/');
    plain.then_some(name)
}

/// The commands and skills whose name starts with what has been typed. Empty unless
/// the input is a bare `/word`.
pub fn matches(value: &str, skills: &[Skill]) -> Vec<Item> {
    let Some(typed) = typing(value) else {
        return Vec::new();
    };
    let typed = typed.to_lowercase();
    let commands = COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(&typed))
        .map(|c| Item {
            name: c.name.to_string(),
            args: c.args.to_string(),
            help: c.help.to_string(),
            skill: false,
        });
    let found = skills
        .iter()
        .filter(|s| s.name.to_lowercase().starts_with(&typed))
        .map(|s| Item {
            name: s.name.clone(),
            args: " [input]".to_string(),
            help: first_sentence(&s.description),
            skill: true,
        });
    let mut items: Vec<Item> = commands.chain(found).collect();
    // An exactly typed name goes first, so enter on `/workflow` does not run `/workflows`.
    if let Some(at) = items.iter().position(|item| item.name == typed) {
        items[..=at].rotate_right(1);
    }
    items
}

/// The skill named by `/<name>`, when there is one. Checked after the commands, so a
/// skill can never shadow one.
pub fn skill<'a>(name: &str, skills: &'a [Skill]) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

/// What `/<skill> [input]` sends: the agent loads the skill through the `skill` tool
/// and follows it, so this is a prompt rather than a command.
pub fn skill_prompt(name: &str, input: &str) -> String {
    match input.is_empty() {
        true => format!("Use the `{name}` skill."),
        false => format!("Use the `{name}` skill.\n\n{input}"),
    }
}

/// What `/help` prints.
pub fn help() -> String {
    let mut out = "commands · type / in the prompt to pick one".to_string();
    for command in COMMANDS {
        out.push_str(&format!(
            "\n  {:<22} {}",
            format!("/{}{}", command.name, command.args),
            command.help
        ));
    }
    out.push_str(concat!(
        "\nkeys",
        "\n  enter send · alt+enter or ctrl+j newline · shift+tab permission mode",
        "\n  ctrl+c interrupt, then quit · ctrl+d quit on an empty prompt",
        "\n  up and down walk the prompt history · tab takes the grey completion",
        "\n  wheel, pgup/pgdn or ctrl+up/down scroll · click a tool output to expand it",
        "\n  drag selects · ctrl+y copies · ctrl+v pastes · ctrl+t shows every badge",
    ));
    out
}

/// A skill description cut to its first sentence, for the one-line menu row.
fn first_sentence(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.find(". ") {
        Some(end) => flat[..end + 1].to_string(),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skills() -> Vec<Skill> {
        ["commit-helper", "pdf"]
            .into_iter()
            .map(|name| Skill {
                name: name.to_string(),
                description: "Does a thing. And then another thing.".to_string(),
                dir: PathBuf::from("/s"),
                source: "~/.claude/skills".to_string(),
            })
            .collect()
    }

    #[test]
    fn the_menu_opens_only_on_a_bare_slash_word() {
        assert_eq!(typing("/"), Some(""));
        assert_eq!(typing("/di"), Some("di"));
        assert_eq!(typing("/workflow x"), None);
        assert_eq!(typing("/usr/bin/env is missing"), None);
        assert_eq!(typing("what is /"), None);
        assert_eq!(typing("/a\nb"), None);
    }

    #[test]
    fn commands_come_before_skills_and_both_filter_by_prefix() {
        let all = matches("/", &skills());
        assert_eq!(all.len(), COMMANDS.len() + 2);
        assert_eq!(all[0].name, "help");
        assert!(all.last().unwrap().skill);

        let c = matches("/c", &skills());
        let names: Vec<_> = c.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            ["compact", "clear", "context", "copy", "commit-helper"]
        );
        assert_eq!(c[4].help, "Does a thing.");
        assert!(c[4].takes_input(), "a skill takes free text");
        assert!(!c[0].takes_input(), "/compact takes nothing");
        assert!(matches("/zzz", &skills()).is_empty());
        assert!(matches("hello", &skills()).is_empty());
    }

    #[test]
    fn an_exactly_typed_name_is_highlighted_over_the_longer_one() {
        let names = |value: &str| -> Vec<String> {
            matches(value, &skills())
                .into_iter()
                .map(|i| i.name)
                .collect()
        };
        assert_eq!(names("/workflow"), ["workflow", "workflows"]);
        assert_eq!(names("/workf"), ["workflows", "workflow"]);
        assert_eq!(names("/pdf"), ["pdf"]);
    }

    #[test]
    fn the_completion_is_the_untyped_tail_of_the_name() {
        let tail = |value: &str| -> Vec<String> {
            matches(value, &skills())
                .iter()
                .map(|item| item.completion(value))
                .collect()
        };
        assert_eq!(tail("/pd"), ["f"]);
        assert_eq!(tail("/pdf"), [""]);
        assert_eq!(tail("/c")[4], "ommit-helper");
        // The menu matches whatever the case, but completing would rewrite the text.
        assert_eq!(tail("/PD"), [""]);
        assert!(tail("/pdf x").is_empty());
    }

    #[test]
    fn a_skill_prompt_carries_the_input() {
        assert_eq!(skill_prompt("pdf", ""), "Use the `pdf` skill.");
        assert_eq!(
            skill_prompt("pdf", "read a.pdf"),
            "Use the `pdf` skill.\n\nread a.pdf"
        );
        assert!(skill("pdf", &skills()).is_some());
        assert!(skill("nope", &skills()).is_none());
    }

    #[test]
    fn help_lists_every_command() {
        let text = help();
        for command in COMMANDS {
            assert!(text.contains(&format!("/{}", command.name)), "{text}");
        }
    }
}
