//! A ```mermaid fence drawn as box-drawing lines instead of shown as its source.
//!
//! `merman-core` parses the diagram into a model and `merman-ascii` lays that model out
//! as text. Only the diagram types `merman-ascii` draws come back; anything else, and
//! anything that does not parse, is `None`, and the caller shows the fence as the code
//! block it would otherwise have been.

use merman_ascii::{AsciiRenderOptions, render_model};
use merman_core::{Engine, ParseOptions};

/// The fence languages that mean mermaid.
pub fn is_mermaid(lang: &str) -> bool {
    matches!(lang, "mermaid" | "mmd")
}

/// The diagram `source` draws as, without trailing spaces, or `None` when it cannot be
/// drawn.
pub fn render(source: &str) -> Option<Vec<String>> {
    // `lenient` answers a diagram that is itself an error message, which would then be
    // drawn as though it were the diagram asked for; `strict` fails so the fence can
    // fall back to its source, which is the more useful thing to show.
    let parsed = Engine::new()
        .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
        .ok()??;
    let drawn = render_model(&parsed.model, &AsciiRenderOptions::unicode()).ok()?;
    let lines: Vec<String> = drawn.lines().map(|l| l.trim_end().to_string()).collect();
    // An empty drawing is no more use than no drawing, and the source says more.
    lines.iter().any(|l| !l.is_empty()).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_diagram_draws_its_participants_and_messages() {
        let lines = render("sequenceDiagram\n    A->>B: hello\n    B-->>A: hi").unwrap();
        let drawn = lines.join("\n");
        for wanted in ["A", "B", "hello", "hi"] {
            assert!(drawn.contains(wanted), "no {wanted} in\n{drawn}");
        }
        assert!(lines.iter().all(|l| l == l.trim_end()), "{drawn}");
    }

    #[test]
    fn a_flowchart_draws_its_nodes() {
        let lines = render("flowchart TD\n    start[Start] --> stop[Stop]").unwrap();
        let drawn = lines.join("\n");
        assert!(drawn.contains("Start") && drawn.contains("Stop"), "{drawn}");
    }

    #[test]
    fn what_cannot_be_drawn_is_left_to_the_caller() {
        for source in ["", "not a mermaid diagram at all", "sequenceDiagram\n  ???"] {
            assert!(render(source).is_none(), "{source:?}");
        }
    }

    #[test]
    fn both_spellings_of_the_fence_are_mermaid() {
        assert!(is_mermaid("mermaid") && is_mermaid("mmd"));
        assert!(!is_mermaid("rust") && !is_mermaid(""));
    }
}
