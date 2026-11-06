//! The command-line grammar, shared by the frontend and offline parser tests.

use clap::{CommandFactory, FromArgMatches as _, Parser, Subcommand};

use super::Args;
use crate::{config::Flags, permissions};

const TOP_HELP: &str = "Run bhai with no command for the terminal UI. -r resumes the latest session here, -r ID a given one; bhai sessions lists them.
exec or --print runs one unattended turn: text on stdout, progress on stderr, --json for JSONL events.
--mode auto lets the judge approve; bypass runs all but ask rules and protected paths (.git, .env, keys, credentials), which still ask, and deny rules, which refuse.
Both need the project trusted (--trust), else the session runs in ask. BHAI_MODE takes ask or auto, never bypass.
SIGTERM or SIGHUP (and SIGINT with --headless) interrupts the turn, keeps the session and prints the -r command that resumes it.
[notify] command in ~/.config/bhai/config.toml runs on approvals, turn ends, failed logins and a 98% used week.

On a server, bhai serve (or --serve PORT --headless) runs sessions with no terminal, under tmux or systemd: see serve --help.
From a laptop, tunnel the port with ssh -L and run bhai attach: see attach --help. The servers listen on 127.0.0.1 only.
Requests carry the token in x-bhai-token; --token-file or BHAI_TOKEN keeps it across restarts.
Over HTTP: GET /state (with quota), GET /events?since=N replays then streams, GET /file and POST /upload move files under the project root.";

#[derive(Debug, Parser)]
#[command(
    name = "bhai",
    version,
    propagate_version = true,
    about = "An agent harness.",
    after_help = TOP_HELP
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Run one prompt without the terminal UI; omit the prompt or use - to read stdin.
    #[arg(short = 'p', long = "print", num_args = 0..=1, default_missing_value = "-", value_name = "PROMPT")]
    print: Option<String>,
    /// Run the entire session in the default Tart VM configured on this host.
    #[arg(long, global = true)]
    tart: bool,
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
    /// Set the permission mode; auto and bypass need a trusted project.
    #[arg(long, global = true, value_parser = ["ask", "auto", "bypass"])]
    mode: Option<String>,
    /// Trust the project as it is now: honour its permission rules and allow auto or bypass.
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
    /// Log model usage, headers and trace spans under .bhai/sessions/<id>/debug.
    #[arg(long, global = true)]
    profile: bool,
    /// Refuse requests that break the prompt cache.
    #[arg(long, global = true)]
    strict_cache: bool,
    /// Start the debug HTTP server on 127.0.0.1 (default port: 7878).
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "7878", value_name = "PORT")]
    serve: Option<u16>,
    /// Run the debug server without the terminal UI; SIGINT ends it as SIGTERM does.
    #[arg(long, global = true, requires = "serve")]
    headless: bool,
    /// Read the server token from this file, or create it (mode 600) with a new one; overrides BHAI_TOKEN.
    #[arg(long, global = true, value_name = "PATH")]
    token_file: Option<std::path::PathBuf>,
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
        after_help = "Text output goes to stdout, progress to stderr. --json emits session events as JSONL.\nApprovals left at ask are rejected; configure allow rules or select --mode auto (with --trust in a project not yet trusted).\n-r [ID] continues a saved session. A failed or interrupted turn exits 1.\n\nWith --serve [PORT] (after the prompt, or as --serve=PORT) the debug server serves the run:\nan approval left at ask waits for POST /approve or /reject (or bhai attach) and fires the [notify] command,\n/prompt joins or queues behind the turn, and /steer, /interrupt and /events work.\nThe process exits once the turn and anything queued behind it have ended, with the usual status.\nThere is no terminal UI to leave out, so --headless is refused.\n  bhai exec 'fix the flaky test' --serve 7878 --token-file ~/.config/bhai/server-token"
    )]
    Exec {
        /// Instructions for the agent, or - to read stdin.
        prompt: String,
    },
    /// Open the terminal UI on a session another bhai serves with --serve.
    #[command(after_help = ATTACH_HELP)]
    Attach {
        /// The server: a port, 127.0.0.1:PORT, localhost:PORT or an http:// URL (default: 7878); with --ssh, its port there.
        target: Option<String>,
        /// The server's token; --token-file reads it from a file, else BHAI_TOKEN.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// The session to show when the server is bhai serve, by its id or a prefix of it.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Reach the server over ssh to DEST (user@host): attach runs the tunnel and restarts it when it drops.
        #[arg(long, value_name = "DEST")]
        ssh: Option<String>,
        /// An argument for ssh, before DEST; repeat it: --ssh-arg=-i --ssh-arg=~/key.pem.
        #[arg(
            long = "ssh-arg",
            value_name = "ARG",
            requires = "ssh",
            allow_hyphen_values = true
        )]
        ssh_args: Vec<String>,
        /// With --ssh and no token given, read it from this file on the server (default: ~/.config/bhai/server-token).
        #[arg(long, value_name = "PATH", requires = "ssh")]
        remote_token_file: Option<String>,
    },
    /// Serve several sessions from one headless process, opened and closed over HTTP.
    #[command(after_help = SERVE_HELP)]
    Serve {
        /// The port on 127.0.0.1 (default: 7878).
        port: Option<u16>,
        /// Open a session in the project root at startup, with --model, --mode, --resume and the rest.
        #[arg(long)]
        open: bool,
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

const ATTACH_HELP: &str = "The server listens on 127.0.0.1 only. --ssh DEST runs the ssh tunnel to it and restarts it when it drops; without it, run your own.
Against bhai serve, --session ID picks a session; without it the only one is used, or you choose from a list.
Leaving (ctrl+c twice, ctrl+d, /quit) only detaches: the session keeps running. esc twice interrupts its turn.
To stop a session, DELETE /sessions/ID on bhai serve, or SIGTERM the bhai that serves it; bhai -r ID resumes it there.
A dropped or silent connection is retried every second and replays from the last event seen; the prompt's border says when it is down and why.
Prompts, answers and settings given while it is down are held and sent once it is back; an interrupt is refused then.
!commands, @ files, pasted images and the commands that act on this machine are off; GET /file and POST /upload move files.
The token comes from --token, then --token-file (which must exist), then BHAI_TOKEN; with --ssh and none of them, from --remote-token-file over ssh.
ssh runs with BatchMode=yes, so it needs a key or an agent; --ssh-arg passes one argument before DEST and repeats.

On the server, in tmux or as the systemd unit in the README:
  bhai serve 7878 --token-file ~/.config/bhai/server-token --open
On the laptop:
  bhai attach 7878 --ssh ubuntu@HOST --ssh-arg=-i --ssh-arg=~/key.pem";

const SERVE_HELP: &str = "Each session runs as its own bhai --serve --headless, in any project; the daemon opens, lists and stops them.
  GET /sessions         id, title, project, working, pending approval, created, updated
  POST /sessions/new    optional JSON: project, model, mode, effort, resume (an id or latest), trust, as
  DELETE /sessions/ID   stops one as SIGTERM would; POST /sessions/ID/close does the same
  /sessions/ID/ROUTE    any --serve route of that session: state, events, prompt, approve, file, upload, ...
Unprefixed routes reach the --open session while it runs, else the only one open.
--model, --mode, --resume and the other session options shape the --open session; --profile, --bare and the like apply to all.
An approval waits for /approve (or bhai attach) and fires [notify]. auto and bypass need trust: --trust, \"trust\": true or POST /sessions/ID/trust.
SIGTERM, SIGHUP or SIGINT stops every session, each with its 10s wind-down, then exits; {\"resume\": ID} or bhai -r ID picks one up again.
Under systemd, set KillMode=mixed so only the daemon gets the SIGTERM (the README has a unit).
  bhai serve 7878 --token-file ~/.config/bhai/server-token --open --mode auto --trust
  bhai attach 7878 --token-file ~/.config/bhai/remote-token --session ID";

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

/// The global options each command's help shows; the rest still parse there, for
/// `into_args` to refuse or ignore, but would only mislead in its help.
const SHOWN: &[(&str, &[&str])] = &[
    ("attach", &["token_file"]),
    (
        "serve",
        &[
            "cd",
            "model",
            "effort",
            "identity",
            "resume",
            "mode",
            "trust",
            "no_global",
            "no_project",
            "bare",
            "profile",
            "strict_cache",
            "token_file",
        ],
    ),
    (
        "exec",
        &[
            "tart",
            "cd",
            "json",
            "model",
            "effort",
            "identity",
            "resume",
            "mode",
            "trust",
            "no_global",
            "no_project",
            "bare",
            "profile",
            "strict_cache",
            "serve",
            "token_file",
        ],
    ),
];

impl Cli {
    /// The grammar, with each command's help trimmed to the global options it takes.
    /// clap copies a global option into a command only where it has none of that id, so
    /// a hidden copy put there first is the one the command parses and leaves out of help.
    pub fn grammar() -> clap::Command {
        let mut grammar = Self::command();
        let globals: Vec<clap::Arg> = grammar
            .get_arguments()
            .filter(|arg| arg.is_global_set())
            .cloned()
            .collect();
        for (name, shown) in SHOWN {
            grammar = grammar.mut_subcommand(name, |mut command| {
                for arg in &globals {
                    if !shown.contains(&arg.get_id().as_str()) {
                        command = command.arg(arg.clone().hide(true));
                    } else if *name == "attach" && arg.get_id() == "token_file" {
                        // attach only reads the file; it never makes one.
                        command =
                            command.arg(arg.clone().help("Read the server's token from this file"));
                    }
                }
                command
            });
        }
        grammar
    }

    /// Parse `args`, the program name first, with the help [`Self::grammar`] gives.
    pub fn parse<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::from_arg_matches(&Self::grammar().try_get_matches_from(args)?)
    }

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
            tart: self.tart,
            command: self.command,
            cd: self.cd,
            probe: self.probe,
            cache_check: self.cache_check,
            judge_eval: self.judge_eval,
            serve: self.serve,
            headless: self.headless,
            token_file: self.token_file,
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
        if parsed.exec.is_some() && parsed.headless {
            return Err(Self::invalid(
                "exec and --print never draw the terminal UI, so drop --headless; --serve alone serves the run",
            ));
        }
        if parsed.exec.is_some() && (parsed.workflow.is_some() || parsed.pick) {
            return Err(Self::invalid(
                "exec and --print run on their own, without --workflow or --pick",
            ));
        }
        let utility = matches!(
            parsed.command,
            Some(
                Command::Identities
                    | Command::Usage
                    | Command::Sessions { .. }
                    | Command::Mcp { .. }
            )
        );
        let attach = matches!(parsed.command, Some(Command::Attach { .. }));
        if (utility || attach) && (parsed.serve.is_some() || parsed.headless) {
            return Err(Self::invalid(
                "--serve and --headless run a session; this command does not start one",
            ));
        }
        if let Some(Command::Serve { open, .. }) = parsed.command {
            if parsed.serve.is_some() || parsed.headless {
                return Err(Self::invalid(
                    "bhai serve is always headless; give the port as bhai serve PORT",
                ));
            }
            if parsed.exec.is_some() || parsed.workflow.is_some() || parsed.pick || parsed.json {
                return Err(Self::invalid(
                    "bhai serve opens sessions over HTTP, without exec, --workflow, --pick or --json",
                ));
            }
            let session = parsed.model.is_some()
                || parsed.effort.is_some()
                || parsed.flags.mode.is_some()
                || parsed.resume.is_some()
                || parsed.identity.is_some()
                || parsed.trust;
            if session && !open {
                return Err(Self::invalid(
                    "--model, --effort, --mode, --resume, --as and --trust are for the session --open starts; POST /sessions/new takes them per session",
                ));
            }
        }
        let daemon = matches!(parsed.command, Some(Command::Serve { .. }));
        if parsed.token_file.is_some() && parsed.serve.is_none() && !attach && !daemon {
            return Err(Self::invalid(
                "--token-file goes with --serve, serve or attach",
            ));
        }
        if attach
            && (parsed.exec.is_some()
                || parsed.workflow.is_some()
                || parsed.resume.is_some()
                || parsed.pick
                || parsed.json
                || parsed.tart
                || parsed.cd.is_some()
                || parsed.model.is_some()
                || parsed.effort.is_some()
                || parsed.identity.is_some()
                || parsed.flags.mode.is_some()
                || parsed.trust
                || parsed.profile
                || parsed.strict_cache
                || parsed.flags.no_global
                || parsed.flags.no_project
                || parsed.flags.bare)
        {
            return Err(Self::invalid(
                "attach shows a session another bhai runs, so it takes no session options",
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
        Cli::parse(std::iter::once("bhai").chain(args.iter().copied()))?.into_args()
    }

    #[test]
    fn tart_is_available_before_and_after_exec() {
        for args in [
            vec!["--tart", "-C", "/guest/project", "exec", "review"],
            vec!["exec", "review", "--tart", "-C", "/guest/project"],
            vec!["--tart", "exec", "review", "--tart", "-C", "/guest/project"],
        ] {
            let parsed = parse(&args).unwrap();
            assert!(parsed.tart);
            assert_eq!(
                parsed.cd.as_deref(),
                Some(std::path::Path::new("/guest/project"))
            );
        }
        assert!(!parse(&["exec", "--", "--tart"]).unwrap().tart);
    }

    #[test]
    fn grammar_is_consistent() {
        Cli::command().debug_assert();
        Cli::grammar().debug_assert();
    }

    #[test]
    fn command_help_shows_only_the_options_that_apply() {
        // The options section alone: the after_help names other commands' flags.
        let help = |name: &str| {
            let mut grammar = Cli::grammar();
            grammar.build();
            let help = grammar
                .find_subcommand_mut(name)
                .unwrap()
                .render_help()
                .to_string();
            let options = help.split("Options:").nth(1).unwrap_or_default();
            options.split("\n\n").next().unwrap_or_default().to_string()
        };
        let attach = help("attach");
        for shown in ["--token-file", "--token ", "--session"] {
            assert!(attach.contains(shown), "{shown} missing:\n{attach}");
        }
        for hidden in [
            "--tart",
            "--json",
            "--model",
            "--resume",
            "--pick",
            "--serve",
            "--headless",
        ] {
            assert!(!attach.contains(hidden), "{hidden} shown:\n{attach}");
        }
        let serve = help("serve");
        assert!(
            serve.contains("--open") && serve.contains("--token-file") && serve.contains("--model")
        );
        for hidden in ["  --tart", "  --json", "--pick", "  --serve", "--headless"] {
            assert!(!serve.contains(hidden), "{hidden} shown:\n{serve}");
        }
        let exec = help("exec");
        assert!(
            exec.contains("--serve") && !exec.contains("--pick") && !exec.contains("--headless")
        );
        let top = Cli::grammar().render_help().to_string();
        assert!(top.contains("--pick") && top.contains("--headless"));
    }

    #[test]
    fn help_and_version_are_available_at_every_level() {
        for args in [
            vec!["--help"],
            vec!["exec", "--help"],
            vec!["attach", "--help"],
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

    #[test]
    fn attach_takes_a_target_and_a_token_and_no_session_options() {
        let args = parse(&["attach", "8791", "--token-file", "/t"]).unwrap();
        assert_eq!(
            args.command,
            Some(Command::Attach {
                target: Some("8791".to_string()),
                token: None,
                session: None,
                ssh: None,
                ssh_args: Vec::new(),
                remote_token_file: None,
            })
        );
        assert_eq!(args.token_file.as_deref(), Some(std::path::Path::new("/t")));
        let args = parse(&["attach", "--token", "abc"]).unwrap();
        assert_eq!(
            args.command,
            Some(Command::Attach {
                target: None,
                token: Some("abc".to_string()),
                session: None,
                ssh: None,
                ssh_args: Vec::new(),
                remote_token_file: None,
            })
        );
        let args = parse(&["attach", "7878", "--session", "ab12"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Attach { session: Some(ref id), .. }) if id == "ab12"
        ));
        let args = parse(&[
            "attach",
            "8791",
            "--ssh",
            "ubuntu@host",
            "--ssh-arg=-i",
            "--ssh-arg",
            "-p",
            "--ssh-arg=2222",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Attach { ssh: Some(ref dest), ref ssh_args, .. })
                if dest == "ubuntu@host" && ssh_args == &["-i", "-p", "2222"]
        ));
        for refused in [
            vec!["attach", "--ssh-arg=-v"],
            vec!["attach", "--remote-token-file", "/t"],
            vec!["attach", "--serve"],
            vec!["attach", "--headless"],
            vec!["attach", "--resume"],
            vec!["attach", "--json"],
            vec!["attach", "-m", "x"],
            vec!["attach", "--mode", "auto"],
            vec!["--tart", "attach"],
            vec!["attach", "1", "2"],
            vec!["--token-file", "/t"],
        ] {
            assert!(parse(&refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn serve_takes_a_port_and_session_options_only_with_open() {
        let args = parse(&["serve", "9000", "--token-file", "/t", "-C", "/p"]).unwrap();
        assert_eq!(
            args.command,
            Some(Command::Serve {
                port: Some(9000),
                open: false
            })
        );
        assert_eq!(args.token_file.as_deref(), Some(std::path::Path::new("/t")));
        let args = parse(&["serve", "--open", "-m", "ollama:x", "--mode", "auto", "-r"]).unwrap();
        assert_eq!(args.model.as_deref(), Some("ollama:x"));
        assert_eq!(args.resume, Some(None));
        assert!(parse(&["serve", "--profile"]).is_ok());
        for refused in [
            vec!["serve", "--serve"],
            vec!["serve", "--headless"],
            vec!["serve", "-m", "x"],
            vec!["serve", "--resume"],
            vec!["serve", "--json"],
            vec!["serve", "--pick"],
            vec!["serve", "nope"],
        ] {
            assert!(parse(&refused).is_err(), "{refused:?}");
        }
    }
}
