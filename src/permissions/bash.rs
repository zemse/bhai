//! A small shell tokenizer for permission checks. It understands just enough bash to
//! split a command into simple commands and words, and refuses everything else.

/// One simple command: its words after quote removal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Command {
    pub words: Vec<String>,
    /// A word globs a dot name (`.e*`), so it may expand to a protected path.
    pub dot_glob: bool,
}

/// Commands that run their arguments as shell code, or as someone else.
const REFUSED: &[&str] = &[
    "eval", "exec", "source", ".", "sudo", "doas", "command", "builtin",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "csh", "tcsh"];
/// Prefixes that run the rest of the words as a command.
const WRAPPERS: &[&str] = &[
    "env", "nohup", "time", "timeout", "nice", "xargs", "stdbuf", "!",
];
/// `find` flags whose arguments, up to a `;` or `+` word, are a command of their own.
const EXEC_FLAGS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// Split `input` into simple commands, or `None` for anything this parser does not
/// fully understand (substitutions, subshells, redirection to files, `eval`, ...).
pub fn parse(input: &str) -> Option<Vec<Command>> {
    let commands = Tokenizer::default().run(input)?;
    commands.iter().all(allowed).then_some(commands)
}

impl Command {
    /// The words from the actual program on, skipping wrappers like `env` or `timeout 5`.
    pub fn unwrapped(&self) -> &[String] {
        let mut words = self.words.as_slice();
        while let Some(first) = words.first()
            && WRAPPERS.contains(&basename(first))
        {
            words = &words[1..];
            while let Some(arg) = words.first()
                && (arg.starts_with(['-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9'])
                    || arg.contains('='))
            {
                words = &words[1..];
            }
        }
        words
    }

    /// The commands this one runs as arguments: `find ... -exec git push \;` runs `git push`.
    pub fn nested(&self) -> Vec<Command> {
        let words = self.unwrapped();
        if words.first().map(|w| basename(w)) != Some("find") {
            return Vec::new();
        }
        let mut nested = Vec::new();
        let mut rest = &words[1..];
        while let Some(at) = rest.iter().position(|w| EXEC_FLAGS.contains(&w.as_str())) {
            // The argument list ends at a `;` or `+` word, or at the end of the command.
            rest = &rest[at + 1..];
            let end = rest
                .iter()
                .position(|w| w == ";" || w == "+")
                .unwrap_or(rest.len());
            let (command, tail) = rest.split_at(end);
            if !command.is_empty() {
                nested.push(Command {
                    words: command.to_vec(),
                    dot_glob: false,
                });
            }
            rest = tail;
        }
        nested
    }
}

/// The last path component of a command name.
pub fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn allowed(command: &Command) -> bool {
    let Some(first) = command.words.first() else {
        return true;
    };
    if is_assignment(first) || REFUSED.contains(&basename(first)) {
        return false;
    }
    // A refused program behind `find -exec` is refused too.
    if !command.nested().iter().all(allowed) {
        return false;
    }
    // `sh -c`, also behind wrappers such as `xargs sh -c`.
    !command.words.iter().enumerate().any(|(i, word)| {
        SHELLS.contains(&basename(word))
            && command.words[i + 1..]
                .iter()
                .any(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
    })
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Default)]
struct Tokenizer {
    commands: Vec<Command>,
    current: Command,
    word: String,
    in_word: bool,
    glob: bool,
}

impl Tokenizer {
    fn run(mut self, input: &str) -> Option<Vec<Command>> {
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => self.end_word(),
                '\n' | ';' => self.end_command(),
                '&' => match chars.next() {
                    Some('&') => self.end_command(),
                    // `&>file` redirects both streams.
                    Some('>') if !self.in_word => redirect(&mut chars)?,
                    // A lone `&` backgrounds the command.
                    _ => return None,
                },
                '|' => {
                    if chars.peek() == Some(&'&') {
                        return None;
                    }
                    chars.next_if_eq(&'|');
                    self.end_command();
                }
                '\'' => {
                    self.in_word = true;
                    loop {
                        match chars.next()? {
                            '\'' => break,
                            c => self.word.push(c),
                        }
                    }
                }
                '"' => {
                    self.in_word = true;
                    loop {
                        match chars.next()? {
                            '"' => break,
                            '$' | '`' => return None,
                            '\\' => match chars.next()? {
                                '\n' => {}
                                c @ ('$' | '`' | '"' | '\\') => self.word.push(c),
                                c => {
                                    self.word.push('\\');
                                    self.word.push(c);
                                }
                            },
                            c => self.word.push(c),
                        }
                    }
                }
                '\\' => match chars.next()? {
                    '\n' => {}
                    c => {
                        self.in_word = true;
                        self.word.push(c);
                    }
                },
                // A lone `$` is literal; anything after it is an expansion.
                '$' if chars.peek().is_none_or(|c| c.is_whitespace()) => {
                    self.in_word = true;
                    self.word.push('$');
                }
                '>' => {
                    if self.in_word && self.word.chars().all(|c| c.is_ascii_digit()) {
                        // A file descriptor such as the `2` in `2>&1`.
                        self.word.clear();
                        self.in_word = false;
                    }
                    self.end_word();
                    redirect(&mut chars)?;
                }
                // An empty pair of braces is literal in bash: the `{}` of `find -exec`.
                '{' if chars.peek() == Some(&'}') => {
                    chars.next();
                    self.in_word = true;
                    self.word.push_str("{}");
                }
                '$' | '`' | '(' | ')' | '<' | '{' | '}' => return None,
                '#' if !self.in_word => while chars.next_if(|c| *c != '\n').is_some() {},
                c => {
                    self.glob |= matches!(c, '*' | '?' | '[');
                    self.in_word = true;
                    self.word.push(c);
                }
            }
        }
        self.end_command();
        Some(self.commands)
    }

    fn end_word(&mut self) {
        if !self.in_word {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let dotted = |part: &str| part.starts_with('.') && part != "." && part != "..";
        if self.glob && word.split('/').any(dotted) {
            self.current.dot_glob = true;
        }
        self.current.words.push(word);
        self.in_word = false;
        self.glob = false;
    }

    fn end_command(&mut self) {
        self.end_word();
        let command = std::mem::take(&mut self.current);
        if !command.words.is_empty() {
            self.commands.push(command);
        }
    }
}

/// The rest of a redirection after its `>`. Only fd duplication (`2>&1`) and
/// `/dev/null` are accepted; writing to any other file is refused.
fn redirect(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<()> {
    chars.next_if(|c| matches!(c, '>' | '|'));
    if chars.next_if_eq(&'&').is_some() {
        let mut digits = 0;
        while chars.next_if(char::is_ascii_digit).is_some() {
            digits += 1;
        }
        let ends = chars
            .peek()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|'));
        return (digits > 0 && ends).then_some(());
    }
    while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
    let mut target = String::new();
    while let Some(c) =
        chars.next_if(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | '<' | '>'))
    {
        target.push(c);
    }
    (target == "/dev/null").then_some(())
}

/// Commands that only read, allowed without a rule in `auto` and `bypass`.
pub fn is_read_only(words: &[String]) -> bool {
    let Some(name) = words.first() else {
        return false;
    };
    let args = &words[1..];
    let has = |f: &dyn Fn(&str) -> bool| args.iter().any(|a| f(a));
    match name.as_str() {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "grep" | "stat" | "which" | "echo"
        | "uname" | "whoami" | "id" | "uptime" | "df" | "du" | "free" | "basename" | "dirname"
        | "realpath" | "readlink" | "nl" | "cut" | "tr" | "column" | "cmp" | "diff" | "md5sum"
        | "sha256sum" | "ps" => true,
        // `--pre` runs a preprocessor program.
        "rg" => !has(&|a| a.starts_with("--pre")),
        // `-C` compiles a magic file, `-o` writes the listing to a file.
        "file" => !has(&|a| is_short_flag(a, 'C')),
        "tree" => !has(&|a| is_short_flag(a, 'o')),
        // `-s` sets the clock.
        "date" => !has(&|a| is_short_flag(a, 's') || a.starts_with("--set")),
        // An argument sets the hostname, and `env cmd` runs cmd.
        "env" | "hostname" => args.is_empty(),
        // `-o` writes the sorted output to a file.
        "sort" => !has(&|a| is_short_flag(a, 'o') || a.starts_with("--output")),
        // A second operand is the file the output goes to.
        "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        // `-f` runs a filter program this cannot see.
        "jq" => !has(&|a| is_short_flag(a, 'f') || a.starts_with("--from-file")),
        "sed" => sed_reads_only(args),
        "awk" => awk_reads_only(args),
        "find" => !has(&|a| {
            matches!(
                a,
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls"
            ) || a.starts_with("-fprint")
        }),
        "git" => match args {
            [sub] if matches!(sub.as_str(), "branch" | "remote") => true,
            [sub, flag] if sub == "remote" && matches!(flag.as_str(), "-v" | "--verbose") => true,
            [sub, flag, ..] if sub == "config" && flag.starts_with("--get") => true,
            [sub, rest @ ..]
                if matches!(
                    sub.as_str(),
                    "status"
                        | "diff"
                        | "log"
                        | "show"
                        | "rev-parse"
                        | "ls-files"
                        | "blame"
                        | "describe"
                ) =>
            {
                !rest
                    .iter()
                    .any(|a| a.starts_with("--output") || a == "--ext-diff")
            }
            _ => false,
        },
        // `--config` can set a runner that executes anything.
        "cargo" => {
            matches!(args.first().map(String::as_str), Some("metadata" | "tree"))
                && !has(&|a| a.starts_with("--config"))
        }
        _ => false,
    }
}

/// `sed` only reads when it edits no file in place, its script is one this can see, and
/// that script has no `w` or `W` command writing a file.
fn sed_reads_only(args: &[String]) -> bool {
    // `-l` takes the next word as its line length, which would hide the script.
    let unseen = |a: &String| {
        is_short_flag(a, 'i')
            || is_short_flag(a, 'f')
            || is_short_flag(a, 'l')
            || a.starts_with("--in-place")
            || a.starts_with("--file")
    };
    if args.iter().any(unseen) {
        return false;
    }
    let mut scripts: Vec<&str> = Vec::new();
    let mut args = args.iter();
    let mut expressions = false;
    while let Some(arg) = args.next() {
        if arg == "-e" || arg == "--expression" {
            expressions = true;
            let Some(script) = args.next() else {
                return false;
            };
            scripts.push(script);
        } else if let Some(script) = arg.strip_prefix("--expression=") {
            expressions = true;
            scripts.push(script);
        } else if !expressions && scripts.is_empty() && !arg.starts_with('-') {
            // With no `-e`, the first operand is the script.
            scripts.push(arg);
        }
    }
    // Telling a `w` command from a `w` inside a pattern needs a sed parser, so any is enough.
    !scripts.is_empty() && scripts.iter().all(|s| !s.contains(['w', 'W']))
}

/// `awk` only reads when its program is one this can see and that program neither shells
/// out nor writes.
fn awk_reads_only(args: &[String]) -> bool {
    let mut program: Option<&str> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.starts_with("-f")
            || arg.starts_with("--file")
            || arg.starts_with("--source")
            || arg.starts_with("-i")
            || arg.starts_with("--include")
            || arg.starts_with("-l")
            || arg.starts_with("--load")
        {
            // The program comes from a file this cannot see.
            return false;
        }
        // `-F sep` takes the next word, which would otherwise look like the program.
        if arg == "-v" || arg == "--assign" || arg == "-F" || arg == "--field-separator" {
            args.next();
        } else if !arg.starts_with('-') && program.is_none() {
            program = Some(arg);
        }
    }
    // `system(` runs a shell command, `>` writes a file and `|` pipes into one.
    program.is_some_and(|p| !p.contains("system(") && !p.contains(['>', '|']))
}

/// Build and test commands a trusted project may run without a rule in `auto`. They run
/// the project's own code, which is what trusting a project means. `inside` says whether
/// an argument names a path within the project.
pub fn is_project_command(words: &[String], inside: &dyn Fn(&str) -> bool) -> bool {
    let Some(name) = words.first().map(|w| basename(w)) else {
        return false;
    };
    let args = &words[1..];
    // An argument naming a path has to stay in the project: `cargo test --manifest-path
    // /elsewhere/Cargo.toml` builds someone else's code.
    // A `~` the shell expands to the home directory leaves the project whatever follows it.
    let escapes = |a: &String| {
        a.starts_with('~') || ((a.contains('/') || a.contains("..")) && !inside(a.as_str()))
    };
    if args.iter().any(escapes) {
        return false;
    }
    match (name, args) {
        ("cargo", [sub, rest @ ..])
            if matches!(
                sub.as_str(),
                "build" | "check" | "test" | "fmt" | "clippy" | "run"
            ) =>
        {
            // `--config` can set a runner that executes anything.
            !rest.iter().any(|a| a.starts_with("--config"))
        }
        ("npm" | "pnpm" | "yarn", [sub, ..]) => {
            matches!(sub.as_str(), "run" | "test" | "install")
        }
        ("go", [sub, ..]) => matches!(sub.as_str(), "build" | "test"),
        ("python3", [file, ..]) => !file.starts_with('-') && inside(file.as_str()),
        ("pytest" | "make" | "just", _) => true,
        _ => false,
    }
}

fn is_short_flag(arg: &str, flag: char) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag)
}

/// Whether any word names a protected path, or may glob into one.
pub fn mentions_protected(command: &Command) -> bool {
    command.dot_glob
        || command.words.iter().any(|word| {
            let lower = word.to_lowercase();
            let parts: Vec<&str> = lower.split(['/', '=', ':', ',']).collect();
            parts.iter().any(|p| {
                matches!(*p, ".git" | ".ssh" | ".codex" | ".claude") || p.starts_with(".env")
            }) || parts.windows(2).any(|w| {
                matches!(
                    w,
                    [".config", "bhai"]
                        | [".bhai", "config.toml"]
                        | [".bhai", "settings.local.json"]
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(input: &str) -> Option<Vec<Vec<String>>> {
        parse(input).map(|cs| cs.into_iter().map(|c| c.words).collect())
    }

    #[test]
    fn splits_into_simple_commands() {
        let cases: &[(&str, &[&[&str]])] = &[
            ("ls", &[&["ls"]]),
            (
                "git status; rm -rf ~",
                &[&["git", "status"], &["rm", "-rf", "~"]],
            ),
            ("a && b || c | d", &[&["a"], &["b"], &["c"], &["d"]]),
            ("a\nb", &[&["a"], &["b"]]),
            (r#"echo "a;b""#, &[&["echo", "a;b"]]),
            ("echo 'a && b' c", &[&["echo", "a && b", "c"]]),
            (r"echo a\;b", &[&["echo", "a;b"]]),
            (r#"echo "x\"y" 'it''s'"#, &[&["echo", "x\"y", "its"]]),
            ("g''it   status", &[&["git", "status"]]),
            ("ls \\\n -la", &[&["ls", "-la"]]),
            ("cargo build 2>&1 | tail", &[&["cargo", "build"], &["tail"]]),
            ("ls >/dev/null 2> /dev/null", &[&["ls"]]),
            ("ls # ; rm x", &[&["ls"]]),
            ("echo a#b $", &[&["echo", "a#b", "$"]]),
            ("ls ;; ", &[&["ls"]]),
            ("echo ''", &[&["echo", ""]]),
        ];
        for (input, want) in cases {
            let want: Vec<Vec<String>> = want
                .iter()
                .map(|c| c.iter().map(|w| w.to_string()).collect())
                .collect();
            assert_eq!(words(input), Some(want), "{input}");
        }
    }

    #[test]
    fn fails_closed_on_what_it_does_not_understand() {
        for input in [
            "ls $(rm x)",
            "ls `rm x`",
            "echo \"$(rm x)\"",
            "echo \"`rm x`\"",
            "echo $HOME",
            "echo ${X}",
            "echo $'\\x41'",
            "diff <(ls) b",
            "tee >(rm x)",
            "echo 'unbalanced",
            "echo \"unbalanced",
            "echo trailing\\",
            "eval ls",
            "exec ls",
            "source x.sh",
            ". x.sh",
            "sh -c 'rm x'",
            "/bin/bash -lc ls",
            "xargs zsh -c ls",
            "cat <<EOF\nx\nEOF",
            "cat < file",
            "echo x > out.txt",
            "echo x >> out.txt",
            "echo x>out.txt",
            "ls &> out",
            "ls 2>&1>out",
            "ls >&out",
            "FOO=1 ls",
            "sudo ls",
            "ls & rm x",
            "ls |& cat",
            "(rm x)",
            "{ rm x; }",
            "ls; command eval x",
            r"find . -exec sudo rm -rf / \;",
            r"find . -execdir eval x \;",
        ] {
            assert_eq!(parse(input), None, "{input}");
        }
    }

    #[test]
    fn unwrapped_skips_wrappers_and_their_flags() {
        let unwrapped = |input: &str| parse(input).unwrap()[0].unwrapped().join(" ");
        assert_eq!(unwrapped("timeout 5s rm -rf x"), "rm -rf x");
        assert_eq!(unwrapped("env -i A=1 /bin/rm x"), "/bin/rm x");
        assert_eq!(unwrapped("xargs -n 1 rm"), "rm");
        assert_eq!(unwrapped("! rm x"), "rm x");
        assert_eq!(unwrapped("git status"), "git status");
    }

    #[test]
    fn nested_commands_behind_find_exec() {
        let nested = |input: &str| {
            parse(input).unwrap()[0]
                .nested()
                .iter()
                .map(|c| c.words.join(" "))
                .collect::<Vec<_>>()
        };
        assert_eq!(nested(r"find . -name x -exec git push \;"), ["git push"]);
        assert_eq!(nested("find . -execdir rm {} +"), ["rm {}"]);
        assert_eq!(
            nested(r"find . -ok rm {} \; -okdir /bin/chmod 777 {} \;"),
            ["rm {}", "/bin/chmod 777 {}"]
        );
        // No terminator: the rest of the words are the command.
        assert_eq!(nested("timeout 5 find . -exec rm x"), ["rm x"]);
        assert_eq!(nested("find . -name x"), [] as [String; 0]);
        assert_eq!(nested("git push"), [] as [String; 0]);
    }

    #[test]
    fn project_commands_run_the_project_on_itself() {
        let inside = |arg: &str| !arg.starts_with('/') && !arg.starts_with("../");
        let project = |input: &str| is_project_command(&parse(input).unwrap()[0].words, &inside);
        for input in [
            "cargo test",
            "cargo clippy --all-targets",
            "cargo fmt",
            "npm run build",
            "yarn install",
            "pnpm test",
            "pytest -q",
            "python3 fizzbuzz.py",
            "python3 scripts/run.py",
            "go test ./...",
            "make",
            "just fmt",
        ] {
            assert!(project(input), "{input}");
        }
        for input in [
            "cargo publish",
            "cargo test --config x",
            "cargo test --manifest-path /other/Cargo.toml",
            "cargo",
            "npm publish",
            "go run x",
            "python3 -c print",
            "python3 ../outside.py",
            "python3 ~/evil.py",
            "make -f ~/Makefile",
            "curl example.com",
            "rm -rf x",
        ] {
            assert!(!project(input), "{input}");
        }
    }

    #[test]
    fn read_only_commands() {
        let read_only = |input: &str| is_read_only(&parse(input).unwrap()[0].words);
        for input in [
            "ls -la",
            "rg foo src",
            "find . -name x",
            "git status",
            "git log --oneline",
            "git branch",
            "tree -L 2",
            "file x",
            "date -u",
            "env",
            "uname -a",
            "df -h",
            "ps aux",
            "cut -d, -f1 x",
            "sort x",
            "uniq x",
            "sha256sum x",
            "diff a b",
            "sed -n 1,5p src/main.rs",
            "sed -e s/a/b/ x",
            r#"awk '{print $1}' x"#,
            "jq .a x.json",
            "git rev-parse HEAD",
            "git remote -v",
            "git config --get user.name",
            "cargo metadata --format-version 1",
            "cargo tree -d",
        ] {
            assert!(read_only(input), "{input}");
        }
        for input in [
            "rm x",
            "cargo test",
            "cargo check",
            "find . -delete",
            "find . -exec rm x ;",
            "find . -fprint out",
            "rg --pre cat x",
            "git -c x=y log",
            "git branch -D main",
            "git logx",
            "git diff --output=x",
            "tree -o out",
            "file -C -m x",
            "/bin/ls",
            "date -s 12:00",
            "env cargo test",
            "hostname other",
            "sort -o out x",
            "uniq a b",
            "sed -i s/a/b/ x",
            "sed s/a/b/w out x",
            "sed -f script.sed x",
            r#"awk '{system("rm x")}' x"#,
            r#"awk '{print > "out"}' x"#,
            "awk -f prog.awk x",
            "jq -f prog.jq x",
            r#"awk -F '{print}' '{system("rm x")}' x"#,
            r#"sed -l 5 's/a/b/w out' x"#,
            "git remote add o u",
            "git config user.name x",
            "cargo metadata --config x",
        ] {
            assert!(!read_only(input), "{input}");
        }
    }

    #[test]
    fn protected_mentions() {
        let protected = |input: &str| parse(input).unwrap().iter().any(mentions_protected);
        for input in [
            "cat .env",
            "cat .ENV.local",
            "ls ~/.ssh",
            "cat /r/.git/config",
            "cat '.gi'\"t\"/HEAD",
            "grep x --file=.env",
            "ls ~/.config/bhai",
            "cat .bhai/config.toml",
            "cp x .bhai/settings.local.json",
            "ls .claude",
            "cat .e*",
            "ls ~/.s?h",
            "ls; cat .env",
        ] {
            assert!(protected(input), "{input}");
        }
        for input in [
            "cat .gitignore",
            "ls .github",
            "ls ../src/*.rs",
            "cat src/env.rs",
        ] {
            assert!(!protected(input), "{input}");
        }
    }
}
