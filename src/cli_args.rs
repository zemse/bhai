//! The command-line grammar, shared by the frontend and offline parser tests.
//!
//! Each command declares only the options it reads, so clap's help and errors for it are
//! the whole story. [`Cli::into_args`] folds the result into the flat [`Args`] the
//! frontend runs from.

use clap::{Args as Group, CommandFactory, Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

use super::Args;
use crate::{config::Flags, permissions};

const SESSION: &str = "Session options";
const MODEL: &str = "Model options";
const PERMISSIONS: &str = "Permission options";
const INSTRUCTIONS: &str = "Instruction options";
const SERVER: &str = "Server and remote control";
const DEBUGGING: &str = "Debugging options";

const EXAMPLES: &str = "Examples:
  bhai                        open the terminal UI in this project
  bhai -r                     resume the latest session here (bhai ls lists them)
  bhai exec 'fix the test'    run one unattended turn and exit
  bhai ls minimised           list sessions running without a terminal
  bhai attach ID              return to a running session
  bhai serve 7878 --open      run headless sessions for bhai attach over ssh

Run bhai help COMMAND, or bhai COMMAND --help, for each command's options.";

const TOP_HELP: &str = "Examples:
  bhai                        open the terminal UI in this project
  bhai -r                     resume the latest session here (bhai ls lists them)
  bhai exec 'fix the test'    run one unattended turn and exit
  bhai ls minimised           list sessions running without a terminal
  bhai attach ID              return to a running session
  bhai serve 7878 --open      run headless sessions for bhai attach over ssh

Run bhai help COMMAND, or bhai COMMAND --help, for each command's options.

Sessions:
  /minimise (or F3) returns to the shell and leaves the session running; /quit stops it.
  SIGTERM or SIGHUP (and SIGINT with --headless) interrupts the turn, keeps the session
  and prints the -r command that resumes it.

Permissions:
  ask approves each tool call. auto lets the judge approve. bypass runs everything except
  ask rules and protected paths (.git, .env, keys, credentials), which still ask, and deny
  rules, which refuse. auto and bypass need a trusted project (--trust), else the session
  runs in ask. BHAI_MODE sets ask or auto, never bypass.

Serving:
  --serve [PORT] serves this session on 127.0.0.1; requests carry x-bhai-token, which
  --token-file or BHAI_TOKEN keeps across restarts. GET /state, GET /events?since=N,
  POST /prompt, /approve, /reject, /interrupt; GET /file and POST /upload move files.
  --rc https://RELAY publishes the session to a relay for phone control, using
  ~/.bhai/control-key.pem; /remote-control [URL] does it at runtime.

Notifications:
  The [notify] command in ~/.config/bhai/config.toml runs on approvals, turn ends,
  failed logins and a 98% used week.";

const EXEC_HELP: &str =
    "Text goes to stdout, progress to stderr; --json writes session events as JSON lines.
Approvals left at ask are rejected: configure allow rules or pass --mode auto (with --trust
in a project not yet trusted). -r [ID] continues a saved session. A failed or interrupted
turn exits 1. A prompt that starts with - follows the terminator: bhai exec -- '-x'.

With --serve [PORT] the debug server serves the run: an approval left at ask waits for
POST /approve or /reject (or bhai attach) and fires the [notify] command, /prompt joins or
queues behind the turn, and /steer, /interrupt and /events work. The process exits once
the turn and anything queued behind it have ended.
  bhai exec 'fix the flaky test' --serve 7878 --token-file ~/.config/bhai/server-token";

const ATTACH_HELP: &str = "Run bhai ls to list live local sessions, then bhai attach ID (a unique nonnumeric prefix also works).
Native terminal sessions keep their original UI, draft and agent: /minimise detaches; /quit stops the session.
Only one native terminal may attach at a time. Terminal or SSH loss detaches without stopping the agent.
Headless sessions attach by ID through their authenticated local API, with the HTTP-view limitations below.
Private sessions are not listed or attached by ID; leave private mode before minimising.

For port/URL attachment, the server listens on 127.0.0.1 only. --ssh DEST runs the ssh tunnel to it and restarts it when it drops; without it, run your own.
Against bhai serve, --session ID picks a session; without it the only one is used, or you choose from a list.
Leaving (ctrl+c twice, ctrl+d, /quit) only detaches: the session keeps running. esc interrupts its turn; enter on an empty prompt resumes it.
To stop a session, DELETE /sessions/ID on bhai serve, or SIGTERM the bhai that serves it; bhai -r ID resumes it there.
It opens on the session as it stands: transcript, subagent panes, plan, monitors, background list, schedules, totals and any approval waiting.
A dropped or silent connection is retried every second and replays from the last event seen; the prompt's border says when it is down and why.
Prompts, answers and settings given while it is down are held and sent once it is back; an interrupt is refused then.
!commands, @ files, pasted images and the commands that act on this machine are off; GET /file and POST /upload move files.
/context prints the server's breakdown; /model lists this machine's models, since the server does not say which it serves.
The token comes from --token, then --token-file (which must exist), then BHAI_TOKEN; with --ssh and none of them, from --remote-token-file over ssh.
ssh runs with BatchMode=yes, so it needs a key or an agent; --ssh-arg passes one argument before DEST and repeats.

On the server, in tmux or as the systemd unit in the README:
  bhai serve 7878 --token-file ~/.config/bhai/server-token --open
On the laptop:
  bhai attach 7878 --ssh ubuntu@HOST --ssh-arg=-i --ssh-arg=~/key.pem";

const COMPLETIONS_HELP: &str = "zsh, with ~/.zfunc on fpath before compinit runs in ~/.zshrc:
  mkdir -p ~/.zfunc && bhai completions zsh > ~/.zfunc/_bhai
bash:
  bhai completions bash > ~/.local/share/bash-completion/completions/bhai
fish:
  bhai completions fish > ~/.config/fish/completions/bhai.fish
Regenerate after upgrading bhai, so new commands and options complete.";

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

/// What shapes a session wherever one starts: the terminal UI, exec, a workflow and
/// the session serve --open starts.
#[derive(Debug, Default, Clone, PartialEq, Group)]
struct Common {
    /// Select the model, including ollama:<name>; bhai models lists them.
    #[arg(short = 'm', long, value_name = "MODEL", help_heading = MODEL)]
    model: Option<String>,
    /// Set the model's reasoning effort.
    #[arg(long, value_name = "LEVEL", help_heading = MODEL)]
    effort: Option<String>,
    /// Start a new session as a configured agent identity; bhai identities lists them.
    #[arg(long = "as", value_name = "IDENTITY", help_heading = MODEL)]
    identity: Option<String>,
    /// How tool calls are approved; auto and bypass need a trusted project.
    #[arg(long, value_parser = ["ask", "auto", "bypass"], help_heading = PERMISSIONS)]
    mode: Option<String>,
    /// Trust the project as it is now: honour its permission rules and allow auto or bypass.
    #[arg(long, help_heading = PERMISSIONS)]
    trust: bool,
    /// Skip global instruction files.
    #[arg(long, help_heading = INSTRUCTIONS)]
    no_global: bool,
    /// Skip project instruction files.
    #[arg(long, help_heading = INSTRUCTIONS)]
    no_project: bool,
    /// Disable instruction files and optional integrations.
    #[arg(long, help_heading = INSTRUCTIONS)]
    bare: bool,
    /// Run the session inside the default Tart VM configured on this host.
    #[arg(long, help_heading = SESSION)]
    tart: bool,
    /// Log model usage, headers and trace spans under .bhai/sessions/<id>/debug.
    #[arg(long, help_heading = DEBUGGING)]
    profile: bool,
    /// Refuse requests that break the prompt cache.
    #[arg(long, help_heading = DEBUGGING)]
    strict_cache: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Group)]
struct Resume {
    /// Resume a saved session, or the latest here when no ID is given; bhai ls lists them.
    #[arg(short = 'r', long, num_args = 0..=1, value_name = "ID", help_heading = SESSION)]
    resume: Option<Option<String>>,
}

/// Serving a session that this process runs.
#[derive(Debug, Default, Clone, PartialEq, Group)]
struct Serving {
    /// Serve the session over HTTP on 127.0.0.1 (default port: 7878); bhai attach PORT opens it.
    #[arg(long, num_args = 0..=1, default_missing_value = "7878", value_name = "PORT", help_heading = SERVER)]
    serve: Option<u16>,
    /// Publish the session to a remote-control relay over HTTPS, for control from a phone.
    #[arg(long, value_name = "ADDRESS", help_heading = SERVER)]
    rc: Option<String>,
    /// Keep the server's token in this file, created (mode 600) when missing; overrides BHAI_TOKEN.
    #[arg(long, value_name = "PATH", requires = "serve", help_heading = SERVER)]
    token_file: Option<PathBuf>,
}

#[derive(Debug, Parser)]
#[command(
    name = "bhai",
    version,
    about = "An agent harness: a terminal UI over a model that runs tools.",
    override_usage = "bhai [OPTIONS]            open the terminal UI\n       bhai [OPTIONS] -p PROMPT  run one unattended turn\n       bhai COMMAND [OPTIONS]",
    after_help = EXAMPLES,
    after_long_help = TOP_HELP
)]
pub(super) struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    root: Root,
    #[command(flatten)]
    legacy: Legacy,
}

/// The options bhai takes with no command: those of the terminal UI and --print.
#[derive(Debug, Group)]
struct Root {
    /// Use this directory as the project root.
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    cd: Option<PathBuf>,
    #[command(flatten)]
    resume: Resume,
    /// Choose the saved session to resume from a list.
    #[arg(long, conflicts_with = "resume", help_heading = SESSION)]
    pick: bool,
    /// Run one unattended turn, as bhai exec does; omit the prompt or use - to read stdin.
    #[arg(short = 'p', long = "print", num_args = 0..=1, default_missing_value = "-", value_name = "PROMPT", help_heading = SESSION)]
    print: Option<String>,
    /// With --print, write session events as JSON lines.
    #[arg(long, help_heading = SESSION)]
    json: bool,
    #[command(flatten)]
    common: Common,
    #[command(flatten)]
    serving: Serving,
    /// Run with no terminal UI, reached only through --serve or --rc.
    #[arg(long, help_heading = SERVER)]
    headless: bool,
}

/// The forms the subcommands replaced, kept so scripts and units keep working. They are
/// left out of completions, which would offer them although hidden.
#[derive(Debug, Group)]
struct Legacy {
    #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "127.0.0.1:8787")]
    relay: Option<SocketAddr>,
    #[arg(long, hide = true, num_args = 1..=2, value_names = ["NAME", "INPUT"])]
    workflow: Option<Vec<String>>,
    #[arg(long, hide = true, requires = "workflow")]
    workflow_yes: bool,
    #[arg(long, hide = true, num_args = 0..=1)]
    probe: Option<Option<String>>,
    #[arg(long, hide = true, num_args = 0..=1, value_parser = cache_wait)]
    cache_check: Option<Option<Duration>>,
    #[arg(long, hide = true, num_args = 0..=1)]
    judge_eval: Option<Option<String>>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run one unattended turn and exit; use - as the prompt to read stdin.
    #[command(after_help = EXEC_HELP)]
    Exec {
        /// Instructions for the agent, or - to read stdin.
        prompt: String,
        /// Write session events as JSON lines instead of text.
        #[arg(long, help_heading = SESSION)]
        json: bool,
        #[command(flatten)]
        resume: Resume,
        #[command(flatten)]
        common: Common,
        #[command(flatten)]
        serving: Serving,
    },
    /// List live sessions on this machine and saved sessions in this project.
    #[command(visible_alias = "ls")]
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCommand>,
    },
    /// Return to a live session by ID, or open the UI on a served port or URL.
    #[command(after_help = ATTACH_HELP)]
    Attach(Attach),
    /// Serve many headless sessions from one process, opened and closed over HTTP.
    #[command(after_help = SERVE_HELP)]
    Serve {
        /// The port on 127.0.0.1 (default: 7878).
        port: Option<u16>,
        /// Open a session in the project root at startup, shaped by the session options.
        #[arg(long)]
        open: bool,
        /// Keep the server's token in this file, created (mode 600) when missing; overrides BHAI_TOKEN.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        #[command(flatten)]
        resume: Resume,
        #[command(flatten)]
        common: Common,
    },
    /// Run a workflow without the terminal UI; without --yes it only prints the plan.
    Workflow {
        /// The workflow's name.
        name: String,
        /// Input for the workflow.
        input: Option<String>,
        /// Confirm the plan and run it.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        resume: Resume,
        #[command(flatten)]
        common: Common,
    },
    /// List model IDs, reasoning efforts and context windows.
    #[command(
        after_help = "Pass models[].id to --model and an efforts[].name to --effort.\nCached Codex results are labelled in notes; no agent session or inference call is started."
    )]
    Models {
        /// Print one JSON object with the models and discovery notes.
        #[arg(long)]
        json: bool,
    },
    /// List configured agent identities.
    Identities,
    /// Show the subscription's usage windows and credits.
    Usage,
    /// Approve an MCP server or manage its OAuth login.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Run the remote-control relay that --rc and /remote-control connect to.
    #[command(
        after_help = "It needs no model credentials. Put an HTTPS reverse proxy in front of its loopback listener."
    )]
    Relay {
        /// The address to listen on.
        #[arg(default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
    },
    /// Print a shell completion script.
    #[command(after_help = COMPLETIONS_HELP)]
    Completions {
        /// The shell to complete for.
        shell: clap_complete::Shell,
    },
    /// Check model auth, prompt caching or the approval judge.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Debug, PartialEq, Group)]
struct Attach {
    /// A live local session ID or unique nonnumeric prefix, or a server port/URL (default: 7878); with --ssh, its port there.
    target: Option<String>,
    /// The server's token; --token-file reads it from a file, else BHAI_TOKEN.
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
    /// Read the server's token from this file.
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,
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
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Check model auth and tool-result replay without executing tools.
    Probe {
        /// The prompt to send.
        prompt: Option<String>,
    },
    /// Check prefix caching, optionally again after a number of minutes.
    CacheCheck {
        /// Minutes to wait before checking the cache lasted.
        #[arg(value_parser = cache_wait)]
        minutes: Option<Duration>,
    },
    /// Score the approval judge against a file of cases.
    JudgeEval {
        /// The cases; the built-in set without one.
        file: Option<String>,
    },
}

/// A command the frontend dispatches on; the rest of the command line is in [`Args`].
#[derive(Debug, PartialEq)]
pub(super) enum Command {
    Exec {
        prompt: String,
    },
    Attach {
        target: Option<String>,
        token: Option<String>,
        session: Option<String>,
        ssh: Option<String>,
        ssh_args: Vec<String>,
        remote_token_file: Option<String>,
    },
    Serve {
        port: Option<u16>,
        open: bool,
    },
    Models,
    Identities,
    Usage,
    Sessions {
        command: Option<SessionsCommand>,
    },
    Mcp {
        command: McpCommand,
    },
    Completions {
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub(super) enum SessionsCommand {
    /// Remove old saved sessions, keeping the newest N (default: 20).
    Prune { keep: Option<usize> },
    /// List only live sessions with no terminal attached.
    #[command(visible_alias = "minimized")]
    Minimised,
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

fn cache_wait(value: &str) -> Result<Duration, String> {
    let minutes = value
        .trim_end_matches('m')
        .parse::<u64>()
        .map_err(|_| "expected a number of minutes".to_string())?;
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| "minutes exceed the supported duration".to_string())?;
    Ok(Duration::from_secs(seconds))
}

impl Common {
    /// Options given after the command win over those before it.
    fn or(self, before: Self) -> Self {
        Self {
            model: self.model.or(before.model),
            effort: self.effort.or(before.effort),
            identity: self.identity.or(before.identity),
            mode: self.mode.or(before.mode),
            trust: self.trust || before.trust,
            no_global: self.no_global || before.no_global,
            no_project: self.no_project || before.no_project,
            bare: self.bare || before.bare,
            tart: self.tart || before.tart,
            profile: self.profile || before.profile,
            strict_cache: self.strict_cache || before.strict_cache,
        }
    }

    fn shapes_a_session(&self) -> bool {
        self.model.is_some()
            || self.effort.is_some()
            || self.identity.is_some()
            || self.mode.is_some()
            || self.trust
    }
}

impl Cli {
    /// The grammar without the hidden legacy flags, for completion scripts.
    pub fn completions() -> clap::Command {
        use clap::{Args as _, Subcommand as _};
        Cmd::augment_subcommands(Root::augment_args(clap::Command::new("bhai")))
    }

    /// Parse `args`, the program name first.
    pub fn parse<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::try_parse_from(args)
    }

    pub fn into_args(self) -> Result<Args, clap::Error> {
        let Cli {
            command,
            root,
            legacy,
        } = self;
        let Root {
            cd,
            resume,
            pick,
            print,
            json,
            common,
            serving,
            headless,
        } = root;
        let Legacy {
            relay,
            workflow,
            workflow_yes,
            probe,
            cache_check,
            judge_eval,
        } = legacy;
        let legacy = [
            relay.is_some(),
            workflow.is_some(),
            probe.is_some(),
            cache_check.is_some(),
            judge_eval.is_some(),
        ]
        .into_iter()
        .filter(|on| *on)
        .count();
        let runs_here = resume != Resume::default()
            || pick
            || print.is_some()
            || serving != Serving::default()
            || headless;
        if legacy > 1 || legacy == 1 && (command.is_some() || runs_here && workflow.is_none()) {
            return Err(Self::invalid(
                "--relay, --workflow and the diagnostic flags each run on their own",
            ));
        }
        if relay.is_some() && common != Common::default() {
            return Err(Self::invalid(
                "the relay runs no session, so it takes no session options",
            ));
        }
        if workflow.is_some()
            && (serving != Serving::default() || headless || pick || print.is_some())
        {
            return Err(Self::invalid(
                "a workflow runs on its own, without --serve, --rc, --headless, --pick or --print",
            ));
        }
        let mut args = Args {
            tart: false,
            command: None,
            cd,
            probe,
            cache_check,
            judge_eval,
            serve: serving.serve,
            rc: serving.rc,
            relay,
            headless,
            token_file: serving.token_file,
            profile: false,
            strict_cache: false,
            identity: None,
            model: None,
            effort: None,
            trust: false,
            resume: resume.resume,
            pick,
            workflow: workflow.map(|values| {
                let mut values = values.into_iter();
                (
                    values.next().unwrap_or_default(),
                    values.next().unwrap_or_default(),
                )
            }),
            workflow_yes,
            exec: print,
            json,
            flags: Flags::default(),
        };
        let before = common;
        let common = match command {
            None => before,
            Some(Cmd::Exec {
                prompt,
                json,
                resume,
                common,
                serving,
            }) => {
                if args.exec.is_some() {
                    return Err(Self::invalid("exec and --print are alternatives"));
                }
                if args.pick || args.headless {
                    return Err(Self::invalid(
                        "exec never draws the terminal UI, so it takes no --pick or --headless; --serve alone serves the run",
                    ));
                }
                args.exec = Some(prompt.clone());
                args.command = Some(Command::Exec { prompt });
                args.json |= json;
                args.resume = resume.resume.or(args.resume);
                args.serve = serving.serve.or(args.serve);
                args.rc = serving.rc.or(args.rc);
                args.token_file = serving.token_file.or(args.token_file);
                common.or(before)
            }
            Some(Cmd::Serve {
                port,
                open,
                token_file,
                resume,
                common,
            }) => {
                if args.pick || args.json || args.exec.is_some() || args.headless {
                    return Err(Self::invalid(
                        "bhai serve opens sessions over HTTP, without --pick, --print, --json or --headless",
                    ));
                }
                if args.serve.is_some() || args.rc.is_some() || args.token_file.is_some() {
                    return Err(Self::invalid(
                        "bhai serve is always headless; give the port as bhai serve PORT, and --token-file after serve",
                    ));
                }
                let common = common.or(before);
                args.resume = resume.resume.or(args.resume);
                if (common.shapes_a_session() || args.resume.is_some()) && !open {
                    return Err(Self::invalid(
                        "--model, --effort, --mode, --resume, --as and --trust are for the session --open starts; POST /sessions/new takes them per session",
                    ));
                }
                args.token_file = token_file;
                args.command = Some(Command::Serve { port, open });
                common
            }
            Some(Cmd::Workflow {
                name,
                input,
                yes,
                resume,
                common,
            }) => {
                if args.pick
                    || args.headless
                    || args.json
                    || args.exec.is_some()
                    || args.serve.is_some()
                    || args.rc.is_some()
                {
                    return Err(Self::invalid(
                        "a workflow runs on its own, without --serve, --rc, --headless, --pick or --print",
                    ));
                }
                args.workflow = Some((name, input.unwrap_or_default()));
                args.workflow_yes = yes;
                args.resume = resume.resume.or(args.resume);
                common.or(before)
            }
            Some(other) => {
                if runs_here || args.json || before != Common::default() {
                    return Err(Self::invalid(
                        "this command starts no session; put session options on bhai, exec, workflow or serve --open",
                    ));
                }
                match other {
                    Cmd::Sessions { command } => args.command = Some(Command::Sessions { command }),
                    Cmd::Attach(attach) => {
                        args.token_file = attach.token_file;
                        args.command = Some(Command::Attach {
                            target: attach.target,
                            token: attach.token,
                            session: attach.session,
                            ssh: attach.ssh,
                            ssh_args: attach.ssh_args,
                            remote_token_file: attach.remote_token_file,
                        });
                    }
                    Cmd::Models { json } => {
                        args.json = json;
                        args.command = Some(Command::Models);
                    }
                    Cmd::Identities => args.command = Some(Command::Identities),
                    Cmd::Usage => args.command = Some(Command::Usage),
                    Cmd::Mcp { command } => args.command = Some(Command::Mcp { command }),
                    Cmd::Relay { bind } => args.relay = Some(bind),
                    Cmd::Completions { shell } => {
                        args.command = Some(Command::Completions { shell })
                    }
                    Cmd::Debug { command } => match command {
                        DebugCommand::Probe { prompt } => args.probe = Some(prompt),
                        DebugCommand::CacheCheck { minutes } => args.cache_check = Some(minutes),
                        DebugCommand::JudgeEval { file } => args.judge_eval = Some(file),
                    },
                    Cmd::Exec { .. } | Cmd::Serve { .. } | Cmd::Workflow { .. } => unreachable!(),
                }
                Common::default()
            }
        };
        if common.identity.is_some() && (args.resume.is_some() || args.pick) {
            return Err(Self::invalid(
                "--as starts a new session, so it cannot be used with --resume or --pick",
            ));
        }
        args.tart = common.tart;
        args.profile = common.profile;
        args.strict_cache = common.strict_cache;
        args.identity = common.identity;
        args.model = common.model;
        args.effort = common.effort;
        args.trust = common.trust;
        args.flags = Flags {
            mode: common
                .mode
                .map(|mode| mode.parse::<permissions::Mode>())
                .transpose()
                .map_err(|error| Self::invalid(&error))?,
            no_global: common.no_global,
            no_project: common.no_project,
            bare: common.bare,
        };
        if args.exec.is_some() && args.pick {
            return Err(Self::invalid(
                "--print runs a new turn, so it takes no --pick",
            ));
        }
        if args.headless && args.serve.is_none() && args.rc.is_none() {
            return Err(Self::invalid("--headless requires --serve or --rc"));
        }
        if args.pick && args.headless {
            return Err(Self::invalid("--pick needs the terminal"));
        }
        if args.json && args.exec.is_none() && args.command != Some(Command::Models) {
            return Err(Self::invalid("--json goes with exec, --print or models"));
        }
        if args
            .exec
            .as_deref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            return Err(Self::invalid("the prompt must not be empty"));
        }
        Ok(args)
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
    fn relay_and_remote_control_have_distinct_run_modes() {
        let relay = parse(&["--relay"]).unwrap();
        assert_eq!(relay.relay.unwrap().to_string(), "127.0.0.1:8787");
        assert!(relay.rc.is_none());
        let remote = parse(&["--rc", "relay.example", "--headless"]).unwrap();
        assert_eq!(remote.rc.as_deref(), Some("relay.example"));
        assert!(remote.headless);
        assert!(parse(&["exec", "test", "--rc", "relay.example"]).is_ok());
        for args in [
            vec!["--relay", "--rc", "relay.example"],
            vec!["--relay", "--model", "test-model"],
            vec!["--relay", "--serve"],
            vec!["--relay", "exec", "test"],
            vec!["--rc", "relay.example", "models"],
            vec!["--headless"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
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
    }

    #[test]
    fn command_help_shows_only_the_options_that_apply() {
        // Up to the after_help, which names other commands' flags.
        let help = |name: &str| {
            let mut grammar = Cli::command();
            grammar.build();
            let command = grammar.find_subcommand_mut(name).unwrap();
            let after = command
                .get_after_help()
                .map(|text| text.to_string())
                .unwrap_or_default();
            let help = command.render_help().to_string();
            help.split(after.as_str())
                .next()
                .unwrap_or_default()
                .to_string()
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
        for hidden in ["  --json", "--pick", "  --serve", "--headless", "--rc"] {
            assert!(!serve.contains(hidden), "{hidden} shown:\n{serve}");
        }
        let exec = help("exec");
        assert!(
            exec.contains("--serve") && !exec.contains("--pick") && !exec.contains("--headless")
        );
        let top = Cli::command().render_help().to_string();
        assert!(top.contains("--pick") && top.contains("--headless"));
        for legacy in [
            "--relay",
            "--workflow",
            "--probe",
            "--cache-check",
            "--judge-eval",
        ] {
            assert!(!top.contains(legacy), "{legacy} shown:\n{top}");
        }
        for name in [
            "models",
            "identities",
            "usage",
            "relay",
            "debug",
            "sessions",
        ] {
            let help = help(name);
            for hidden in ["--model", "--resume", "--serve", "--mode"] {
                assert!(!help.contains(hidden), "{hidden} shown for {name}:\n{help}");
            }
        }
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
        assert_eq!(
            parse(&["--version"]).unwrap_err().kind(),
            ErrorKind::DisplayVersion
        );
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
        for name in ["minimised", "minimized"] {
            assert_eq!(
                parse(&["sessions", name]).unwrap().command,
                Some(Command::Sessions {
                    command: Some(SessionsCommand::Minimised)
                })
            );
        }
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
    fn completions_cover_the_commands_and_hide_the_old_flags() {
        assert_eq!(
            parse(&["completions", "zsh"]).unwrap().command,
            Some(Command::Completions {
                shell: clap_complete::Shell::Zsh
            })
        );
        assert!(parse(&["completions", "nope"]).is_err());
        assert!(parse(&["-m", "x", "completions", "zsh"]).is_err());
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ] {
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::completions(), "bhai", &mut script);
            let script = String::from_utf8(script).unwrap();
            for word in ["attach", "minimised", "model", "resume", "completions"] {
                assert!(script.contains(word), "{shell}: {word} missing");
            }
            assert!(
                !script.contains("--workflow-yes"),
                "{shell} completes a hidden flag"
            );
        }
    }

    #[test]
    fn ls_is_sessions() {
        assert_eq!(
            parse(&["ls"]).unwrap().command,
            Some(Command::Sessions { command: None })
        );
        assert_eq!(
            parse(&["ls", "minimised"]).unwrap().command,
            Some(Command::Sessions {
                command: Some(SessionsCommand::Minimised)
            })
        );
    }

    #[test]
    fn run_modes_are_commands_and_their_old_flags_still_parse() {
        assert_eq!(parse(&["relay"]).unwrap(), parse(&["--relay"]).unwrap());
        assert_eq!(
            parse(&["relay", "0.0.0.0:9000"]).unwrap(),
            parse(&["--relay=0.0.0.0:9000"]).unwrap()
        );
        assert_eq!(
            parse(&["workflow", "review", "src", "--yes", "-m", "x"]).unwrap(),
            parse(&["--workflow", "review", "src", "--workflow-yes", "-m", "x"]).unwrap()
        );
        let args = parse(&["workflow", "review"]).unwrap();
        assert_eq!(args.workflow, Some(("review".into(), String::new())));
        assert!(!args.workflow_yes && args.command.is_none());
        assert_eq!(
            parse(&["debug", "probe", "say ok"]).unwrap(),
            parse(&["--probe", "say ok"]).unwrap()
        );
        assert_eq!(
            parse(&["debug", "cache-check", "2"]).unwrap().cache_check,
            Some(Some(std::time::Duration::from_secs(120)))
        );
        assert_eq!(
            parse(&["debug", "judge-eval"]).unwrap().judge_eval,
            Some(None)
        );
        for refused in [
            vec!["relay", "-m", "x"],
            vec!["-m", "x", "relay"],
            vec!["debug", "probe", "-m", "x"],
            vec!["workflow", "review", "--serve"],
            vec!["--serve", "workflow", "review"],
            vec!["--workflow", "review", "--pick"],
            vec!["--relay", "exec", "hi"],
            vec!["-m", "x", "sessions"],
            vec!["-r", "attach", "abc"],
        ] {
            assert!(parse(&refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn models_supports_json_without_an_unattended_prompt() {
        let args = parse(&["models", "--json"]).unwrap();
        assert_eq!(args.command, Some(Command::Models));
        assert!(args.json && args.exec.is_none());
        assert!(parse(&["--json", "models"]).is_err());
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
