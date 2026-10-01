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
        name: "compact-then",
        args: " <prompt>",
        help: "continue from the copy compacted while the cache was warm",
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
        name: "export-debug",
        args: "",
        help: "write everything about this session to one file",
    },
    Command {
        name: "permissions",
        args: "",
        help: "the rules that decide approvals",
    },
    Command {
        name: "allow",
        args: " <rule>",
        help: "let a kind of call run without asking",
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
    // No args hint, though it takes words to filter by: the list is the usual way in.
    Command {
        name: "sessions",
        args: "",
        help: "mention a past session's file, which the agent can read",
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
    // No args hint, though it takes an objective: on its own it shows the goal.
    Command {
        name: "goal",
        args: "",
        help: "work on its own until done, /goal <objective>, pause, resume, budget <n>, clear",
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
        name: "effort",
        args: " <level>",
        help: "the reasoning effort, keeping the model",
    },
    // No args hint, for the same reason as `/model`.
    Command {
        name: "model-default",
        args: "",
        help: "pick the model new sessions start on",
    },
    Command {
        name: "effort-default",
        args: " <level>",
        help: "the effort new sessions start at",
    },
    // No args hint, though it takes a request: on its own it shows the template and
    // the variables, which is where a first look starts.
    Command {
        name: "statusline",
        args: "",
        help: "the status bar, /statusline <what you want> to change it",
    },
    Command {
        name: "mcp",
        args: "",
        help: "the MCP servers and their tools, /mcp reload <server> to restart one",
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
        name: "usage",
        args: "",
        help: "the plan's rate limits and credits",
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
    pub fn completion(&self, before: &str) -> String {
        let Some(typed) = typing(before) else {
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

/// The name being typed at the cursor, given the text before it: that text has to end
/// in a `/word` which starts a word of its own, so a name can be completed mid-sentence
/// while a path, a url or a date in the prompt keeps the menu shut.
pub fn typing(before: &str) -> Option<&str> {
    let (head, name) = before.rsplit_once('/')?;
    let opens = head.is_empty() || head.ends_with(char::is_whitespace);
    let plain = !name.contains(|c: char| c.is_whitespace() || c == '/');
    (opens && plain).then_some(name)
}

/// The commands and skills whose name starts with the `/word` at the cursor. Empty when
/// there is no such word.
pub fn matches(before: &str, skills: &[Skill]) -> Vec<Item> {
    let Some(typed) = typing(before) else {
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
        "\n  ctrl+c interrupt, clear the draft (ctrl+z restores), then quit on a second press · ctrl+d quit on an empty prompt",
        "\n  !<command> runs a shell command yourself, shown here and not told to the model",
        "\n  up and down walk the prompt history · ctrl+r searches it · tab takes the grey completion",
        "\n  up on an empty prompt takes the queued messages back to edit",
        "\n  wheel, pgup/pgdn or ctrl+up/down scroll",
        "\n  click a tool output to expand it · ctrl+l expands or collapses them all",
        "\n  drag selects, past the edge to keep going · ctrl+a takes the whole transcript",
        "\n  ctrl+y copies · ctrl+v pastes · ctrl+g edits the draft in $EDITOR · ctrl+t shows every badge",
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
    fn the_menu_opens_on_a_slash_word_wherever_it_is_typed() {
        assert_eq!(typing("/"), Some(""));
        assert_eq!(typing("/di"), Some("di"));
        // Mid-sentence, on the word the cursor is in.
        assert_eq!(typing("have a look with /di"), Some("di"));
        assert_eq!(typing("what is /"), Some(""));
        assert_eq!(typing("run it\n/di"), Some("di"));
        // The word has ended, so there is nothing to complete.
        assert_eq!(typing("/workflow x"), None);
        assert_eq!(typing("/a\nb"), None);
        // A `/` that starts no word: a path, a url, a date, a fraction.
        assert_eq!(typing("/usr/bin/env is missing"), None);
        assert_eq!(typing("look in src/ma"), None);
        assert_eq!(typing("see https://ex"), None);
        assert_eq!(typing("due 12/09"), None);
        assert_eq!(typing("and/or"), None);
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
            [
                "compact",
                "compact-then",
                "clear",
                "context",
                "copy",
                "commit-helper"
            ]
        );
        assert_eq!(c[5].help, "Does a thing.");
        assert!(c[5].takes_input(), "a skill takes free text");
        assert!(!c[0].takes_input(), "/compact takes nothing");
        assert!(c[1].takes_input(), "/compact-then takes a prompt");
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
        assert_eq!(tail("/c")[5], "ommit-helper");
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
