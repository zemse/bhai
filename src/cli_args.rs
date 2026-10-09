//! The command-line grammar, shared by the frontend and offline parser tests.

use clap::{CommandFactory, Parser, Subcommand};

use super::Args;
use crate::{config::Flags, permissions};

#[derive(Debug, Parser)]
#[command(
    name = "bhai",
    version,
    propagate_version = true,
    about = "An agent harness.",
    after_help = "Run bhai with no command for the terminal UI.\nUse exec or --print for one unattended turn; approvals left at ask are rejected.\nText output goes to stdout, progress to stderr. exec --json emits session events as JSONL.\nUse models --json for a model catalogue other harnesses can read."
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Run one prompt without the terminal UI; omit the prompt or use - to read stdin.
    #[arg(short = 'p', long = "print", num_args = 0..=1, default_missing_value = "-", value_name = "PROMPT")]
    print: Option<String>,
    /// Use this directory as the project root.
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    cd: Option<std::path::PathBuf>,
    /// Print exec events as JSON lines, or the models catalogue as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Select the model (including ollama:<name>).
    #[arg(short = 'm', long, global = true, value_name = "MODEL")]
    model: Option<String>,
    /// Set the model's reasoning effort.
    #[arg(long, global = true, value_name = "LEVEL")]
    effort: Option<String>,
    /// Run as a configured agent identity.
    #[arg(long = "as", global = true, value_name = "IDENTITY", conflicts_with_all = ["resume", "pick"])]
    identity: Option<String>,
    /// Resume a saved session, or the latest when no ID is given.
    #[arg(short = 'r', long, global = true, num_args = 0..=1, value_name = "ID", conflicts_with = "pick")]
    resume: Option<Option<String>>,
    /// Choose a session interactively.
    #[arg(long, global = true)]
    pick: bool,
    /// Set the permission mode (overrides BHAI_MODE).
    #[arg(long, global = true, value_parser = ["ask", "auto", "bypass"])]
    mode: Option<String>,
    /// Honour the project-supplied permission rules as they are now.
    #[arg(long, global = true)]
    trust: bool,
    /// Skip global instruction files.
    #[arg(long, global = true)]
    no_global: bool,
    /// Skip project instruction files.
    #[arg(long, global = true)]
    no_project: bool,
    /// Disable instruction files and optional integrations.
    #[arg(long, global = true)]
    bare: bool,
    /// Log model usage, headers and trace spans under .bhai/debug.
    #[arg(long, global = true)]
    profile: bool,
    /// Refuse requests that break the prompt cache.
    #[arg(long, global = true)]
    strict_cache: bool,
    /// Start the local debug HTTP server (default port: 7878).
    #[arg(long, num_args = 0..=1, default_missing_value = "7878", value_name = "PORT")]
    serve: Option<u16>,
    /// Run the debug server without the terminal UI.
    #[arg(long, requires = "serve")]
    headless: bool,
    /// Run a workflow, with optional input.
    #[arg(long, num_args = 1..=2, value_names = ["NAME", "INPUT"])]
    workflow: Option<Vec<String>>,
    /// Confirm the workflow plan instead of only printing it.
    #[arg(long, requires = "workflow")]
    workflow_yes: bool,
    /// Check model auth and tool-result replay without executing tools.
    #[arg(long, num_args = 0..=1, value_name = "PROMPT", group = "diagnostic")]
    pub probe: Option<Option<String>>,
    /// Check prefix caching, optionally again after a number of minutes.
    #[arg(long, num_args = 0..=1, value_name = "MINUTES", value_parser = cache_wait, group = "diagnostic")]
    pub cache_check: Option<Option<std::time::Duration>>,
    /// Score the approval judge against a file of cases.
    #[arg(long, num_args = 0..=1, value_name = "FILE", group = "diagnostic")]
    pub judge_eval: Option<Option<String>>,
}

#[derive(Debug, PartialEq, Subcommand)]
pub(super) enum Command {
    /// Run one unattended turn; use - as the prompt to read stdin.
    #[command(
        after_help = "Text output goes to stdout, progress to stderr. --json emits session events as JSONL.\nApprovals left at ask are rejected; configure allow rules or explicitly select --mode auto."
    )]
    Exec {
        /// Instructions for the agent, or - to read stdin.
        prompt: String,
    },
    /// List available model IDs, reasoning efforts and context windows.
    #[command(
        after_help = "Use --json for a single object with models and discovery notes.\nPass models[].id to --model and an efforts[].name to --effort.\nCached Codex results are labelled in notes; no agent session or inference call is started."
    )]
    Models,
    /// List configured agent identities.
    Identities,
    /// Show the subscription's usage windows and credits.
    Usage,
    /// List saved sessions in this project.
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCommand>,
    },
    /// Approve an MCP server or manage its OAuth login.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(super) enum SessionsCommand {
    /// Remove old sessions, retaining the newest N (default: 20).
    Prune { keep: Option<usize> },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(super) enum McpCommand {
    /// Approve the server's current project configuration.
    Approve { server: String },
    /// Sign in to a hosted server over OAuth.
    Login { server: String },
    /// Forget a hosted server's OAuth login.
    Logout { server: String },
}

fn cache_wait(value: &str) -> Result<std::time::Duration, String> {
    let minutes = value
        .trim_end_matches('m')
        .parse::<u64>()
        .map_err(|_| "expected a number of minutes".to_string())?;
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| "minutes exceed the supported duration".to_string())?;
    Ok(std::time::Duration::from_secs(seconds))
}

impl Cli {
    pub fn into_args(self) -> Result<Args, clap::Error> {
        if (self.probe.is_some() || self.cache_check.is_some() || self.judge_eval.is_some())
            && (self.command.is_some()
                || self.print.is_some()
                || self.serve.is_some()
                || self.workflow.is_some()
                || self.resume.is_some()
                || self.pick)
        {
            return Err(Self::invalid("diagnostic flags run on their own"));
        }
        let exec = match self.command.as_ref() {
            Some(Command::Exec { prompt }) => {
                if self.print.is_some() {
                    return Err(Self::invalid("exec and --print are alternatives"));
                }
                Some(prompt.clone())
            }
            Some(_) => {
                if self.print.is_some() {
                    return Err(Self::invalid("--print cannot be used with a command"));
                }
                None
            }
            None => self.print,
        };
        let workflow = self.workflow.map(|values| {
            let mut values = values.into_iter();
            (
                values.next().unwrap_or_default(),
                values.next().unwrap_or_default(),
            )
        });
        let parsed = Args {
            command: self.command,
            cd: self.cd,
            probe: self.probe,
            cache_check: self.cache_check,
            judge_eval: self.judge_eval,
            serve: self.serve,
            headless: self.headless,
            profile: self.profile,
            strict_cache: self.strict_cache,
            identity: self.identity,
            model: self.model,
            effort: self.effort,
            trust: self.trust,
            resume: self.resume,
            pick: self.pick,
            workflow,
            workflow_yes: self.workflow_yes,
            exec,
            json: self.json,
            flags: Flags {
                mode: self
                    .mode
                    .map(|mode| mode.parse::<permissions::Mode>())
                    .transpose()
                    .map_err(|error| Self::invalid(&error))?,
                no_global: self.no_global,
                no_project: self.no_project,
                bare: self.bare,
            },
        };
        if parsed.workflow.is_some() && (parsed.serve.is_some() || parsed.headless || parsed.pick) {
            return Err(Self::invalid(
                "--workflow runs on its own, without --serve, --headless or --pick",
            ));
        }
        if parsed.exec.is_some()
            && (parsed.serve.is_some()
                || parsed.headless
                || parsed.workflow.is_some()
                || parsed.pick)
        {
            return Err(Self::invalid(
                "exec and --print run on their own, without --serve, --headless, --workflow or --pick",
            ));
        }
        let models = matches!(parsed.command, Some(Command::Models));
        if models
            && (parsed.serve.is_some()
                || parsed.workflow.is_some()
                || parsed.resume.is_some()
                || parsed.pick)
        {
            return Err(Self::invalid(
                "models lists the catalogue without --serve, --workflow, --resume or --pick",
            ));
        }
        if parsed.json && parsed.exec.is_none() && !models {
            return Err(Self::invalid("--json requires exec, --print or models"));
        }
        if parsed.pick && parsed.headless {
            return Err(Self::invalid("--pick needs the terminal"));
        }
        if parsed
            .exec
            .as_deref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            return Err(Self::invalid("the prompt must not be empty"));
        }
        Ok(parsed)
    }

    fn invalid(message: &str) -> clap::Error {
        Self::command().error(clap::error::ErrorKind::ArgumentConflict, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("bhai").chain(args.iter().copied()))?.into_args()
    }

    #[test]
    fn grammar_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_and_version_are_available_at_every_level() {
        for args in [
            vec!["--help"],
            vec!["exec", "--help"],
            vec!["sessions", "prune", "--help"],
            vec!["mcp", "login", "--help"],
        ] {
            assert_eq!(parse(&args).unwrap_err().kind(), ErrorKind::DisplayHelp);
        }
        for args in [vec!["--version"], vec!["exec", "--version"]] {
            assert_eq!(parse(&args).unwrap_err().kind(), ErrorKind::DisplayVersion);
        }
    }

    #[test]
    fn global_options_work_before_and_after_exec() {
        let before = parse(&[
            "-m",
            "ollama:script",
            "--mode",
            "auto",
            "--json",
            "exec",
            "review",
        ])
        .unwrap();
        let after = parse(&[
            "exec",
            "--json",
            "--mode",
            "auto",
            "-m",
            "ollama:script",
            "review",
        ])
        .unwrap();
        assert_eq!(before, after);
        assert_eq!(after.exec.as_deref(), Some("review"));
        assert_eq!(after.flags.mode, Some(permissions::Mode::Auto));
        assert!(after.json);
    }

    #[test]
    fn print_accepts_a_prompt_or_stdin() {
        assert_eq!(
            parse(&["-p", "review"]).unwrap().exec.as_deref(),
            Some("review")
        );
        assert_eq!(parse(&["--print"]).unwrap().exec.as_deref(), Some("-"));
        assert_eq!(
            parse(&["-p", "-", "--json"]).unwrap().exec.as_deref(),
            Some("-")
        );
        assert!(parse(&["-p", "review", "exec", "other"]).is_err());
        assert!(parse(&["-p", "review", "sessions"]).is_err());
        assert!(parse(&["-p", "review", "--pick"]).is_err());
        assert!(parse(&["exec", "   "]).is_err());
    }

    #[test]
    fn equals_values_and_option_terminators_work() {
        let args = parse(&[
            "exec",
            "--model=ollama:script",
            "--resume=abc",
            "--",
            "--literal prompt",
        ])
        .unwrap();
        assert_eq!(args.model.as_deref(), Some("ollama:script"));
        assert_eq!(args.resume, Some(Some("abc".to_string())));
        assert_eq!(args.exec.as_deref(), Some("--literal prompt"));
        let args = parse(&["-C", "/tmp", "exec", "review"]).unwrap();
        assert_eq!(args.cd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn utility_commands_are_typed_and_reject_extra_arguments() {
        assert_eq!(
            parse(&["sessions", "prune", "5"]).unwrap().command,
            Some(Command::Sessions {
                command: Some(SessionsCommand::Prune { keep: Some(5) })
            })
        );
        assert_eq!(
            parse(&["mcp", "approve", "server"]).unwrap().command,
            Some(Command::Mcp {
                command: McpCommand::Approve {
                    server: "server".to_string()
                }
            })
        );
        for args in [
            vec!["sessions", "bogus"],
            vec!["sessions", "prune", "nope"],
            vec!["identities", "extra"],
            vec!["mcp", "login"],
            vec!["mcp", "login", "server", "extra"],
            vec!["--workflow-yes"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn models_supports_json_without_an_unattended_prompt() {
        let args = parse(&["models", "--json"]).unwrap();
        assert_eq!(args.command, Some(Command::Models));
        assert!(args.json && args.exec.is_none());
        assert_eq!(parse(&["--json", "models"]).unwrap(), args);
        assert_eq!(
            parse(&["models", "--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        for refused in [
            vec!["models", "extra"],
            vec!["--serve", "models"],
            vec!["models", "--resume"],
            vec!["models", "--pick"],
            vec!["--json", "sessions"],
        ] {
            assert!(parse(&refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn diagnostics_preserve_optional_values_and_validate_durations() {
        assert_eq!(parse(&["--probe"]).unwrap().probe, Some(None));
        assert_eq!(
            parse(&["--probe", "say ok"]).unwrap().probe,
            Some(Some("say ok".to_string()))
        );
        assert_eq!(
            parse(&["--cache-check", "2m"]).unwrap().cache_check,
            Some(Some(std::time::Duration::from_secs(120)))
        );
        assert!(parse(&["--cache-check", "nope"]).is_err());
        assert!(parse(&["--cache-check", "18446744073709551615"]).is_err());
        assert!(parse(&["--probe", "--judge-eval"]).is_err());
        assert!(parse(&["--probe", "--print", "hi"]).is_err());
        assert!(parse(&["--probe", "exec", "hi"]).is_err());
    }
}
