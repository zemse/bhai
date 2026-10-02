//! The status bar as a template the user writes: `statusline` in the global config.
//! The syntax is a subset of starship's format strings, which models and users already
//! know: `$name` is a variable, `[text](style)` styles what is inside it, `( ... )`
//! drops what is inside it when every variable in it is empty, and `\` takes the next
//! character as it is. Text a template does not style is dim, like the built-in bar.
//!
//! A variable that is warning, such as the context past three quarters full, keeps its
//! warning colour whatever the template styled it: that colour is what the bar is for.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::app::App;
use crate::client::{Client, Provider, Usage};
use crate::limits::{self, RateLimits};
use crate::tools::BoxFuture;

/// Every variable a template can use, and what it holds.
pub const VARIABLES: &[(&str, &str)] = &[
    ("model", "the model the session talks to"),
    ("effort", "its reasoning effort; empty on Ollama"),
    ("fast", "`fast` while /fast has calls on the priority tier"),
    ("provider", "`codex` or `ollama`"),
    ("identity", "the identity the session runs as"),
    ("mode", "the permission mode: ask, auto or bypass"),
    ("branch", "the git branch of the working directory"),
    ("dir", "the working directory's name"),
    ("cwd", "the working directory, with the home directory as ~"),
    (
        "ctx",
        "how full the context window is, as `42%`; the compacted copy's while the prompt starts with /compact-then",
    ),
    (
        "ctx_used",
        "tokens the last call read, as `12.3k`, or the copy's",
    ),
    ("ctx_window", "the context window, as `272.0k`"),
    ("tokens_in", "input tokens this session, as `1.2M`"),
    ("tokens_out", "output tokens this session"),
    (
        "cache",
        "the share of the last call's input that was cached, as `83%`",
    ),
    (
        "cache_alert",
        "a cache break, a stall or a miss, while there is one",
    ),
    (
        "cache_timer",
        "`cache expires in 4:07`, over the cache's last five minutes",
    ),
    (
        "cache_expired",
        "`cache expired: /clear to save ~86k tokens`, once the cache has likely lapsed",
    ),
    (
        "fork",
        "`fork uncached: ~9k tokens`, while the prompt starts with /compact-then",
    ),
    (
        "limits",
        "every rate-limit window with its reset, as `5h:42% resets@03:10 | 7d:17% resets@Fri 09:00`",
    ),
    ("limit_5h", "the shorter rate-limit window used, as `42%`"),
    ("reset_5h", "when it resets, as `2h14m`"),
    ("limit_week", "the longer rate-limit window used"),
    ("reset_week", "when it resets, as `Fri 09:00`"),
    (
        "credits",
        "the credits left, as `8.3k/10.0k` or `unlimited`",
    ),
    ("credits_left", "the credits left, as `8.3k`"),
    ("credits_used", "the credits spent from the allowance"),
    ("credits_limit", "the allowance, as `10.0k`"),
    ("credits_reset", "when the allowance resets, as `Thu 05:30`"),
    ("working", "`working` while a turn runs"),
    ("queued", "prompts waiting behind the turn, as `2 queued`"),
    ("session", "the first 8 characters of the session id"),
    ("time", "the local time, as `14:05`"),
    ("version", "bhai's version"),
    (
        "hint",
        "the key hint: `/ for commands`, or the approval keys while one waits",
    ),
];

/// A variable's text now, and its warning colour when it is warning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Value {
    pub text: String,
    pub alert: Option<Style>,
}

impl Value {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            alert: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var(String),
    Styled(Vec<Node>, Style),
    /// Dropped when every variable in it is empty.
    Optional(Vec<Node>),
}

/// A parsed template, kept with the text it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub source: String,
    nodes: Vec<Node>,
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut parser = Parser {
            chars: source.chars().collect(),
            at: 0,
        };
        let nodes = parser.nodes(None)?;
        Ok(Self {
            source: source.to_string(),
            nodes,
        })
    }

    /// The bar in at most `width` columns. What does not fit goes a `( ... )` group at a
    /// time from the right, so a row that is short of room loses whole parts rather than
    /// ending halfway through one; past that the edge cuts it.
    pub fn render(&self, values: &HashMap<&str, Value>, width: usize) -> Vec<Span<'static>> {
        let mut hidden = vec![false; groups(&self.nodes)];
        let mut next = hidden.len();
        loop {
            let mut spans = Vec::new();
            let mut at = 0;
            let base = Style::new().fg(Color::DarkGray);
            render(&self.nodes, base, values, &hidden, &mut at, &mut spans);
            let wide = spans.iter().map(Span::width).sum::<usize>() > width;
            if !wide || next == 0 {
                return spans;
            }
            next -= 1;
            hidden[next] = true;
        }
    }
}

/// How many `( ... )` groups there are, nested ones included.
fn groups(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| match node {
            Node::Text(_) | Node::Var(_) => 0,
            Node::Styled(inner, _) => groups(inner),
            Node::Optional(inner) => 1 + groups(inner),
        })
        .sum()
}

/// `hidden` is indexed by each group's place in the template, which `at` counts.
fn render(
    nodes: &[Node],
    style: Style,
    values: &HashMap<&str, Value>,
    hidden: &[bool],
    at: &mut usize,
    out: &mut Vec<Span<'static>>,
) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push(Span::styled(text.clone(), style)),
            Node::Var(name) => {
                if let Some(value) = values.get(name.as_str()).filter(|v| !v.text.is_empty()) {
                    let style = value.alert.map_or(style, |alert| style.patch(alert));
                    out.push(Span::styled(value.text.clone(), style));
                }
            }
            Node::Styled(inner, own) => render(inner, style.patch(*own), values, hidden, at, out),
            Node::Optional(inner) => {
                let this = *at;
                *at += 1;
                if hidden[this] || all_empty(inner, values) {
                    *at += groups(inner);
                } else {
                    render(inner, style, values, hidden, at, out);
                }
            }
        }
    }
}

/// Whether every variable in the group is empty. A group always has one: `parse`
/// refuses a group without.
fn all_empty(nodes: &[Node], values: &HashMap<&str, Value>) -> bool {
    nodes.iter().all(|node| match node {
        Node::Text(_) => true,
        Node::Var(name) => values.get(name.as_str()).is_none_or(|v| v.text.is_empty()),
        Node::Styled(inner, _) | Node::Optional(inner) => all_empty(inner, values),
    })
}

fn has_variable(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| match node {
        Node::Text(_) => false,
        Node::Var(_) => true,
        Node::Styled(inner, _) | Node::Optional(inner) => has_variable(inner),
    })
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    /// Nodes up to `end`, which is consumed, or to the end of the text when `None`.
    fn nodes(&mut self, end: Option<char>) -> Result<Vec<Node>, String> {
        let mut nodes = Vec::new();
        let mut text = String::new();
        let flush = |text: &mut String, nodes: &mut Vec<Node>| {
            if !text.is_empty() {
                nodes.push(Node::Text(std::mem::take(text)));
            }
        };
        loop {
            let Some(c) = self.peek() else {
                return match end {
                    None => {
                        flush(&mut text, &mut nodes);
                        Ok(nodes)
                    }
                    Some(end) => Err(format!("`{end}` is missing at the end")),
                };
            };
            self.at += 1;
            match c {
                c if Some(c) == end => {
                    flush(&mut text, &mut nodes);
                    return Ok(nodes);
                }
                '\\' => match self.peek() {
                    Some(next) => {
                        text.push(next);
                        self.at += 1;
                    }
                    None => return Err("`\\` at the end escapes nothing".to_string()),
                },
                '$' => {
                    flush(&mut text, &mut nodes);
                    nodes.push(Node::Var(self.variable()?));
                }
                '[' => {
                    flush(&mut text, &mut nodes);
                    let inner = self.nodes(Some(']'))?;
                    if self.peek() != Some('(') {
                        return Err(
                            "`[text]` must be followed by `(style)`; write `\\[` for a bracket"
                                .to_string(),
                        );
                    }
                    self.at += 1;
                    let spec: String = self.chars[self.at..]
                        .iter()
                        .take_while(|&&c| c != ')')
                        .collect();
                    self.at += spec.chars().count();
                    if self.peek() != Some(')') {
                        return Err(format!("the style `({spec}` is missing its `)`"));
                    }
                    self.at += 1;
                    nodes.push(Node::Styled(inner, style(&spec)?));
                }
                '(' => {
                    flush(&mut text, &mut nodes);
                    let from = self.at - 1;
                    let inner = self.nodes(Some(')'))?;
                    // Such a group hides nothing, so it is nearly always a style put
                    // somewhere other than after `[text]`, which would print as text.
                    if !has_variable(&inner) {
                        let group: String = self.chars[from..self.at].iter().collect();
                        return Err(format!(
                            "`{group}` has no variable in it, so it hides nothing; a style \
goes after brackets, `[$branch](purple)`, and a literal parenthesis is `\\(`"
                        ));
                    }
                    nodes.push(Node::Optional(inner));
                }
                ']' | ')' => return Err(format!("`{c}` closes nothing; write `\\{c}`")),
                c => text.push(c),
            }
        }
    }

    /// `name` or `{name}` after a `$`, which must be a known variable.
    fn variable(&mut self) -> Result<String, String> {
        let braced = self.peek() == Some('{');
        if braced {
            self.at += 1;
        }
        let name: String = self.chars[self.at..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
            .collect();
        self.at += name.len();
        if braced {
            if self.peek() != Some('}') {
                return Err(format!("`${{{name}` is missing its `}}`"));
            }
            self.at += 1;
        }
        if name.is_empty() {
            return Err("`$` names no variable; write `\\$` for a dollar".to_string());
        }
        if !VARIABLES.iter().any(|(known, _)| *known == name) {
            let known: Vec<&str> = VARIABLES.iter().map(|(name, _)| *name).collect();
            return Err(format!(
                "no variable `${name}`; there is {}",
                known.join(", ")
            ));
        }
        Ok(name)
    }
}

/// A starship style: words such as `bold red`, `fg:cyan bg:#1e1e2e`, `dimmed italic`.
fn style(spec: &str) -> Result<Style, String> {
    let mut style = Style::new();
    for word in spec.split_whitespace() {
        let word = word.to_ascii_lowercase();
        style = match word.as_str() {
            "bold" => style.add_modifier(Modifier::BOLD),
            "italic" => style.add_modifier(Modifier::ITALIC),
            "underline" => style.add_modifier(Modifier::UNDERLINED),
            "dimmed" | "dim" => style.add_modifier(Modifier::DIM),
            "inverted" | "reversed" => style.add_modifier(Modifier::REVERSED),
            "strikethrough" => style.add_modifier(Modifier::CROSSED_OUT),
            word => match word.split_once(':') {
                Some(("fg", colour)) => style.fg(color(colour)?),
                Some(("bg", colour)) => style.bg(color(colour)?),
                Some(_) => return Err(format!("`{word}` is not a style")),
                None => style.fg(color(word)?),
            },
        };
    }
    Ok(style)
}

fn color(name: &str) -> Result<Color, String> {
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
        && let Ok(rgb) = u32::from_str_radix(hex, 16)
    {
        return Ok(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
    }
    if let Ok(index) = name.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }
    let name = name.replace('-', "_");
    Ok(match name.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "purple" | "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "dark_gray" | "dark_grey" | "darkgray" | "bright_black" => Color::DarkGray,
        "bright_red" => Color::LightRed,
        "bright_green" => Color::LightGreen,
        "bright_yellow" => Color::LightYellow,
        "bright_blue" => Color::LightBlue,
        "bright_purple" | "bright_magenta" => Color::LightMagenta,
        "bright_cyan" => Color::LightCyan,
        "bright_white" => Color::White,
        _ => return Err(format!("`{name}` is not a colour")),
    })
}

/// How a percentage used is drawn when it is worth a look: yellow past `WARN`, red and
/// bold past `ALERT`.
fn warning(used_percent: f64) -> Option<Style> {
    if used_percent >= limits::ALERT {
        Some(Style::new().fg(Color::Red).bold())
    } else if used_percent >= limits::WARN {
        Some(Style::new().fg(Color::Yellow))
    } else {
        None
    }
}

/// Every variable's value for the session `app` shows.
pub fn values(app: &App) -> HashMap<&'static str, Value> {
    let compact = crate::ui::compact;
    let mut values: HashMap<&'static str, Value> = HashMap::new();
    let mut set = |name: &'static str, value: Value| {
        values.insert(name, value);
    };
    let provider = Provider::of(&app.model);
    set("model", Value::plain(&app.model));
    set(
        "effort",
        Value::plain(match provider {
            Provider::Codex => app.effort.as_str(),
            Provider::Ollama => "",
        }),
    );
    set(
        "fast",
        Value::plain(match provider == Provider::Codex && app.fast {
            true => "fast",
            false => "",
        }),
    );
    set(
        "provider",
        Value::plain(match provider {
            Provider::Codex => "codex",
            Provider::Ollama => "ollama",
        }),
    );
    set("identity", Value::plain(&app.identity));
    set("mode", Value::plain(app.mode.to_string()));
    set(
        "branch",
        Value::plain(app.branch.name().unwrap_or_default()),
    );
    let cwd = std::env::current_dir().unwrap_or_default();
    set(
        "dir",
        Value::plain(
            cwd.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    );
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    set(
        "cwd",
        Value::plain(
            match home.and_then(|home| cwd.strip_prefix(home).ok().map(|p| p.to_path_buf())) {
                Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                Some(rest) => format!("~/{}", rest.display()),
                None => cwd.display().to_string(),
            },
        ),
    );
    let window = app.limits.window(&app.model);
    set("ctx_window", Value::plain(compact(window)));
    let fork = app.forked();
    match fork.or(app.last_usage.map(|Usage { input, .. }| input)) {
        Some(input) => {
            let percent = 100.0 * input as f64 / window as f64;
            set(
                "ctx",
                Value {
                    text: format!("{percent:.0}%"),
                    alert: warning(percent),
                },
            );
            set("ctx_used", Value::plain(compact(input)));
        }
        None => {
            set("ctx", Value::default());
            set("ctx_used", Value::default());
        }
    }
    let count = |n: u64| match n {
        0 => String::new(),
        n => compact(n),
    };
    set("tokens_in", Value::plain(count(app.tokens_in)));
    set("tokens_out", Value::plain(count(app.tokens_out)));
    set(
        "cache",
        Value::plain(
            app.last_usage
                .and_then(|u| u.cache_rate())
                .map(|rate| format!("{rate:.0}%"))
                .unwrap_or_default(),
        ),
    );
    let cache_alert = match (&app.cache_break, app.cache_stalled, app.cache_miss) {
        (Some(field), _, _) => Value {
            text: format!("cache break: {field}"),
            alert: Some(Style::new().fg(Color::Red).bold()),
        },
        (None, true, _) => Value {
            text: "cache stalled".to_string(),
            alert: Some(Style::new().fg(Color::Yellow).bold()),
        },
        (None, false, Some(percent)) => Value {
            text: format!("cache miss {percent:.0}%"),
            alert: Some(Style::new().fg(Color::Yellow).bold()),
        },
        (None, false, None) => Value::default(),
    };
    set("cache_alert", cache_alert);
    set(
        "cache_timer",
        match app.cache_left().filter(|_| fork.is_none()) {
            Some(left) => Value {
                text: format!("cache expires in {}", crate::ui::clock(left)),
                alert: Some(Style::new().fg(Color::Yellow)),
            },
            None => Value::default(),
        },
    );
    set(
        "cache_expired",
        match app.cold_tokens().filter(|_| fork.is_none()) {
            Some(tokens) => Value {
                text: format!("cache expired: /clear to save ~{} tokens", compact(tokens)),
                alert: Some(Style::new().fg(Color::Yellow)),
            },
            None => Value::default(),
        },
    );
    set(
        "fork",
        match fork {
            Some(tokens) => Value {
                text: format!("fork uncached: ~{} tokens", compact(tokens)),
                alert: Some(Style::new().fg(Color::Yellow)),
            },
            None => Value::default(),
        },
    );
    let limits = app.rate_limits.unwrap_or_default();
    let now = chrono::Local::now();
    set("limits", all_limits(&limits, now));
    for (window, used, reset) in [
        (limits.short(), "limit_5h", "reset_5h"),
        (limits.long(), "limit_week", "reset_week"),
    ] {
        set(
            used,
            window.map_or_else(Value::default, |w| Value {
                text: format!("{:.0}%", w.used_percent),
                alert: warning(w.used_percent),
            }),
        );
        set(
            reset,
            Value::plain(window.and_then(|w| w.resets_in(now)).unwrap_or_default()),
        );
    }
    let credits = limits.credits;
    set(
        "credits",
        credits.map_or_else(Value::default, |c| Value {
            text: c.amount(),
            alert: c.used_percent().and_then(warning),
        }),
    );
    let short = |n: Option<f64>| {
        n.map(|n| compact(n.max(0.0).floor() as u64))
            .unwrap_or_default()
    };
    set(
        "credits_left",
        Value::plain(short(credits.and_then(|c| c.remaining))),
    );
    set(
        "credits_used",
        Value::plain(short(credits.and_then(|c| c.used))),
    );
    set(
        "credits_limit",
        Value::plain(short(credits.and_then(|c| c.limit))),
    );
    set(
        "credits_reset",
        Value::plain(credits.and_then(|c| c.resets_in(now)).unwrap_or_default()),
    );
    set(
        "working",
        Value::plain(if app.working { "working" } else { "" }),
    );
    set(
        "queued",
        Value::plain(match app.queued.len() {
            0 => String::new(),
            n => format!("{n} queued"),
        }),
    );
    set(
        "session",
        Value::plain(app.session_id.chars().take(8).collect::<String>()),
    );
    set("time", Value::plain(now.format("%H:%M").to_string()));
    set("version", Value::plain(env!("CARGO_PKG_VERSION")));
    set(
        "hint",
        Value::plain(match app.pending.is_some() {
            true => "y yes · n no · or click a choice",
            false => "/ for commands",
        }),
    );
    values
}

/// `5h:42% resets@03:10 | 7d:17% resets@Fri 09:00`, in the colour of the window closest
/// to its limit; `5h:none` where the plan has only the long window.
fn all_limits(found: &RateLimits, now: chrono::DateTime<chrono::Local>) -> Value {
    let mut parts = Vec::new();
    let mut worst: f64 = 0.0;
    if found.short().is_none() && found.long().is_some() {
        parts.push("5h:none".to_string());
    }
    let mut windows: Vec<_> = found.windows().collect();
    windows.sort_by_key(|w| w.window_minutes.unwrap_or(u64::MAX));
    for window in windows {
        worst = worst.max(window.used_percent);
        let mut text = format!("{}:{:.0}%", window.label(), window.used_percent);
        if let Some(at) = window.resets_at_clock(now) {
            text.push_str(&format!(" resets@{at}"));
        }
        parts.push(text);
    }
    Value {
        text: parts.join(" | "),
        alert: warning(worst),
    }
}

/// What `/statusline` shows: the template in force and every variable's value now.
pub fn report(app: &App, path: Option<&std::path::Path>) -> String {
    let values = values(app);
    let mut out = match &app.statusline {
        Some(template) => format!("status line: {}\n", template.source),
        None => "status line: the built-in one\n".to_string(),
    };
    if let Some(path) = path {
        out.push_str(&format!("saved in {}\n", path.display()));
    }
    out.push_str(USAGE);
    out.push_str("\n\nvariables, with what they hold now:\n");
    for (name, about) in VARIABLES {
        let now = values
            .get(name)
            .map(|v| v.text.as_str())
            .unwrap_or_default();
        out.push_str(&format!("  ${name:<12} {now:<16} {about}\n"));
    }
    out.trim_end().to_string()
}

pub const USAGE: &str = "\
/statusline <what you want>   the model writes the template
/statusline set <template>    set it yourself
/statusline reset             back to the built-in one
syntax: $var, [text](bold cyan), ( dropped when its variables are empty ), \\ escapes";

/// Fixed for the life of the session, so the call's prefix caches.
fn system() -> String {
    let mut vars = String::new();
    for (name, about) in VARIABLES {
        vars.push_str(&format!("- ${name}: {about}\n"));
    }
    format!(
        "You write the status line template of bhai, a terminal coding agent. The status \
line is one row at the bottom of the terminal. The template is a subset of starship's \
format strings:
- `$name` or `${{name}}` is a variable. Only these exist:
{vars}- `[text](style)` styles what is inside the brackets, which may hold variables. A style \
is words such as `bold`, `italic`, `underline`, `dimmed`, `inverted`, a colour (black red \
green yellow blue purple cyan white gray dark_gray, bright_red and the like, #rrggbb, or 0 \
to 255), `fg:colour` and `bg:colour`.
- A style only ever follows brackets: `[$cwd](bold)`, never `$cwd(bold)` or `(bold)$cwd`. \
Background colours are the same: `[ $model ](fg:black bg:blue)`.
- `( ... )` is dropped when every variable inside it is empty, so separators around a \
value that is not there yet go with it: `( · ctx $ctx)`. It must hold a variable.
- `\\` takes the next character literally: `\\$`, `\\[`, `\\(`.
- Unstyled text is dim. A variable that is warning, such as $ctx past 75%, keeps its \
warning colour whatever the style.
Keep it to one row of about 100 columns. Start from the current template when the user \
asks for a change to it. Answer with the template alone: one line, no quotes, no code \
fence, no explanation.

Example: [ bhai ](fg:black bg:cyan) $model( $effort)( on [$branch](purple))( · ctx $ctx)( · $limits)"
    )
}

/// The model call behind `/statusline <request>`, so tests inject one that never talks
/// to the model. The answer is a template that parses, or why there is none.
pub trait Design: Send + Sync {
    fn design<'a>(&'a self, request: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

/// The cache key of the design call, so its prefix caches on its own.
const CACHE_KEY: &str = "statusline";

/// The designer backed by the session's client.
pub struct ModelDesigner {
    client: Client,
    model: String,
}

impl ModelDesigner {
    pub fn new(client: Client, model: Option<String>) -> Self {
        let model = model.unwrap_or_else(|| client.model().to_string());
        Self { client, model }
    }
}

impl Design for ModelDesigner {
    fn design<'a>(&'a self, request: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let system = system();
            let mut message = request.to_string();
            let mut failure = String::new();
            // A model that got the syntax wrong is told what was wrong once; twice is a
            // sign the request cannot be written as a template.
            for _ in 0..2 {
                let (reply, _usage) = self
                    .client
                    .aside(CACHE_KEY, &self.model, "low", &system, &message)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                let template = tidy(&reply);
                match Template::parse(&template) {
                    Ok(_) => return Ok(template),
                    Err(e) => {
                        failure =
                            format!("the model wrote `{template}`, which does not parse: {e}");
                        message = format!(
                            "{request}\n\nYour last answer was `{template}`, which does not parse: \
{e}. Answer again with a template that does."
                        );
                    }
                }
            }
            Err(failure)
        })
    }
}

/// The template out of a reply that wrapped it in a fence or quotes anyway.
fn tidy(reply: &str) -> String {
    reply
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .unwrap_or_default()
        .trim_matches('`')
        .to_string()
}

/// A designer that answers without a model, for tests.
#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    pub struct Designer {
        answer: Result<String, String>,
        pub asked: Mutex<Vec<String>>,
    }

    impl Designer {
        pub fn new(answer: Result<&str, &str>) -> Arc<Self> {
            Arc::new(Self {
                answer: answer.map(str::to_string).map_err(str::to_string),
                asked: Mutex::default(),
            })
        }
    }

    impl Design for Designer {
        fn design<'a>(&'a self, request: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(request.to_string());
                self.answer.clone()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn with(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, Value> {
        pairs
            .iter()
            .map(|(name, text)| (*name, Value::plain(*text)))
            .collect()
    }

    #[test]
    fn variables_and_text_render_in_order() {
        let template = Template::parse("$model on $branch").unwrap();
        let values = with(&[("model", "gpt-5"), ("branch", "main")]);
        assert_eq!(text(&template.render(&values, 200)), "gpt-5 on main");
        assert_eq!(
            template.render(&values, 200)[1].style.fg,
            Some(Color::DarkGray)
        );
    }

    #[test]
    fn a_group_drops_out_when_its_variables_are_empty() {
        let template = Template::parse("$model( on $branch)( · ctx $ctx) · end").unwrap();
        let values = with(&[("model", "m"), ("branch", ""), ("ctx", "4%")]);
        assert_eq!(text(&template.render(&values, 200)), "m · ctx 4% · end");
    }

    #[test]
    fn a_narrow_row_drops_groups_from_the_right() {
        let template = Template::parse("$model( on $branch)( · ctx $ctx)( · $time)").unwrap();
        let values = with(&[
            ("model", "m"),
            ("branch", "main"),
            ("ctx", "4%"),
            ("time", "14:05"),
        ]);
        let row = |width| text(&template.render(&values, width));
        assert_eq!(row(40), "m on main · ctx 4% · 14:05");
        assert_eq!(row(20), "m on main · ctx 4%");
        assert_eq!(row(10), "m on main");
        // Past the last group the edge cuts what is left.
        assert_eq!(row(3), "m");
    }

    #[test]
    fn styles_nest_and_a_warning_keeps_its_colour() {
        let template = Template::parse("[ctx [$ctx](bold)](fg:cyan bg:#102030)").unwrap();
        let mut values = with(&[]);
        values.insert(
            "ctx",
            Value {
                text: "95%".to_string(),
                alert: Some(Style::new().fg(Color::Red)),
            },
        );
        let spans = template.render(&values, 200);
        assert_eq!(text(&spans), "ctx 95%");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(spans[0].style.bg, Some(Color::Rgb(0x10, 0x20, 0x30)));
        assert_eq!(spans[1].style.fg, Some(Color::Red));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn escapes_and_braced_names() {
        let template = Template::parse(r"\$5 \[${model}\] \(x\)").unwrap();
        let values = with(&[("model", "m")]);
        assert_eq!(text(&template.render(&values, 200)), "$5 [m] (x)");
    }

    #[test]
    fn mistakes_are_named() {
        let bad = |t: &str| Template::parse(t).unwrap_err();
        assert!(
            bad("$nope").contains("no variable `$nope`"),
            "{}",
            bad("$nope")
        );
        assert!(bad("[x]").contains("(style)"));
        assert!(bad("[x](blurple)").contains("not a colour"));
        assert!(bad("(x").contains("`)` is missing"));
        assert!(bad("x)").contains("closes nothing"));
        assert!(bad("${model").contains("missing its `}`"));
        assert!(bad("$branch(purple)").contains("`(purple)` has no variable"));
    }

    #[test]
    fn every_variable_has_a_value() {
        let app = App::detached();
        let values = values(&app);
        for (name, _) in VARIABLES {
            assert!(values.contains_key(name), "${name} has no value");
        }
        assert_eq!(values.len(), VARIABLES.len());
    }

    #[test]
    fn a_fenced_reply_comes_out_as_the_template() {
        assert_eq!(tidy("```\n$model on $branch\n```\n"), "$model on $branch");
        assert_eq!(tidy("`$model`"), "$model");
    }
}
