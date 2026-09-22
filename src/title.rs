//! The terminal's title: the project the session is in, then a few words on what it is
//! doing. A tab is narrow and cuts the front off what it cannot fit, keeping the end, so
//! the half worth reading goes last and the directory goes in front of it.
//!
//! The name comes from one small model call on the session's first message. Only the
//! terminal ever shows it, so a call that fails leaves the directory standing on its own
//! rather than being worth reporting.

use std::io::Write;
use std::path::Path;

use crate::client::{Client, Usage};
use crate::tools::BoxFuture;

/// Longest the model's half of the title may be, in characters. A tab shows nothing like
/// this much, but the escape sequence is not the place to find that out.
const CLIP: usize = 60;
/// The cache key of the naming call, so its prefix caches on its own.
const CACHE_KEY: &str = "title";

/// Fixed for the life of the session, so the naming call's prefix caches.
pub const SYSTEM: &str = "\
You name what a user has asked a coding agent to do, for the title of a terminal tab. \
Very short: a few plain words, lower case, no full stop and no quotes. Name the work, \
not the request: \"fix the judge cache\", not \"the user wants the judge cache fixed\". \
Answer with the name and nothing else.";

/// The model call behind the name, so tests inject one that never talks to the model.
pub trait Name: Send + Sync {
    fn name<'a>(&'a self, message: &'a str) -> BoxFuture<'a, Option<String>>;
}

/// Write the terminal's title. Only the thread that draws may call this: a write from
/// anywhere else can land inside a frame and cut an escape sequence in half.
pub fn set(text: &str) {
    let _ = write(&mut std::io::stdout(), text);
}

/// OSC 0, which sets the window's title and the tab's; OSC 2 sets only the window's, and
/// a tab is what this is for.
fn write(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    // A control character would end the sequence early and print the rest of the title.
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    std::write!(out, "\x1b]0;{text}\x07")?;
    out.flush()
}

/// Ask the terminal to remember the title it had, and to put it back. Not every terminal
/// answers either, and one that does not is left showing the last title bhai set.
pub fn push() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[22;0t");
    let _ = out.flush();
}

pub fn pop() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[23;0t");
    let _ = out.flush();
}

/// The title itself: the project directory, then what the session is doing. The tab
/// keeps the end of what it cannot fit, so the work is what survives being cut.
pub fn compose(root: &Path, work: Option<&str>) -> String {
    let dir = root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string());
    match work {
        Some(work) if !work.trim().is_empty() => format!("{dir} · {}", tidy(work)),
        _ => dir,
    }
}

/// One line of the model's answer, cut to `CLIP`. A model asked for a few words sometimes
/// writes a sentence about them instead, and the tab is no place to find that out either.
fn tidy(text: &str) -> String {
    let line = text
        .trim()
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '`', '.'])
        .trim();
    match line.char_indices().nth(CLIP) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The namer backed by the session's client, on a cache key of its own so its request
/// never disturbs the conversation's cached prefix.
pub struct ModelNamer {
    client: Client,
    model: String,
    effort: String,
}

impl ModelNamer {
    pub fn new(client: Client, model: Option<String>, effort: &str) -> Self {
        let model = model.unwrap_or_else(|| client.model().to_string());
        Self {
            client,
            model,
            effort: effort.to_string(),
        }
    }
}

impl Name for ModelNamer {
    fn name<'a>(&'a self, message: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let (reply, _usage): (String, Usage) = self
                .client
                .aside(CACHE_KEY, &self.model, &self.effort, SYSTEM, message)
                .await
                .ok()?;
            let name = tidy(&reply);
            (!name.is_empty()).then_some(name)
        })
    }
}

/// A namer that answers without a model, for tests.
#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    pub struct Namer {
        name: Option<String>,
        /// Every message it was asked to name, so a test can see it was asked once.
        pub asked: Mutex<Vec<String>>,
    }

    impl Namer {
        pub fn new(name: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                name: name.map(str::to_string),
                asked: Mutex::default(),
            })
        }
    }

    impl Name for Namer {
        fn name<'a>(&'a self, message: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move {
                self.asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message.to_string());
                self.name.clone()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_comes_first_and_the_work_last() {
        let root = Path::new("/home/u/workspace/bhai");
        assert_eq!(compose(root, None), "bhai");
        assert_eq!(
            compose(root, Some("fix the judge cache")),
            "bhai · fix the judge cache"
        );
        // Nothing to say is the directory on its own, not a trailing separator.
        assert_eq!(compose(root, Some("   ")), "bhai");
    }

    #[test]
    fn the_title_goes_out_as_one_escape_sequence() {
        let mut out = Vec::new();
        // A newline in the name would end the sequence and print what followed it.
        write(&mut out, "bhai · fix\nthe cache").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]0;bhai · fixthe cache\x07"
        );
    }

    #[test]
    fn a_model_that_answers_with_more_than_a_name_is_cut_back_to_one() {
        assert_eq!(tidy("  \"fix the judge cache\"  "), "fix the judge cache");
        assert_eq!(
            tidy("fix the judge cache.\n\nWant me to?"),
            "fix the judge cache"
        );
        let long = "a".repeat(CLIP + 10);
        assert_eq!(tidy(&long).chars().count(), CLIP + 1);
        assert!(tidy(&long).ends_with('…'));
    }
}
