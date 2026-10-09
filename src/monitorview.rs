//! Live observer cards above the prompt; full snapshots live in the background inspector.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::monitor::{View, metric_text, track_line};
use crate::ui::clip;

const MAX_LINES: usize = 10;

pub fn height(views: &[View], available: u16) -> u16 {
    if views.is_empty() {
        return 0;
    }
    let lines: usize = views
        .iter()
        .map(|v| {
            1 + v.snapshot.as_ref().map_or(0, |s| {
                s.tracks.len()
                    + usize::from(!s.summary.is_empty())
                    + usize::from(!s.metrics.is_empty())
                    + usize::from(!s.details.is_empty())
            }) + usize::from(v.error.is_some())
        })
        .sum();
    (lines.min(MAX_LINES) as u16 + 2).min(available / 3)
}

pub fn render(frame: &mut Frame, area: Rect, views: &[View], collapsed: bool) {
    if area.height == 0 || views.is_empty() {
        return;
    }
    let dim = Style::new().fg(Color::DarkGray);
    let block = Block::bordered()
        .border_style(dim)
        .title(Line::styled(" monitors · /monitor ", dim))
        .title_top(Line::styled(if collapsed { " ▸ " } else { " ▾ " }, dim).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if collapsed {
        return;
    }
    let width = inner.width as usize;
    let mut lines = Vec::new();
    for view in views {
        let updated = view
            .updated_secs_ago
            .map_or("waiting for first sample".into(), |s| {
                format!("updated {s}s ago")
            });
        lines.push(Line::from(vec![Span::styled(
            clip(
                &format!(" {} · {} [{}] · {updated}", view.name, view.id, view.state),
                width,
            ),
            Style::new().fg(if view.error.is_some() {
                Color::Yellow
            } else {
                Color::Cyan
            }),
        )]));
        if let Some(snapshot) = &view.snapshot {
            if !snapshot.summary.is_empty() {
                lines.push(Line::raw(clip(&format!(" {}", snapshot.summary), width)));
            }
            for track in &snapshot.tracks {
                let bar = width.saturating_sub(track.id.chars().count() + 28).min(20);
                lines.push(Line::raw(clip(
                    &format!(" {}", track_line(track, bar)),
                    width,
                )));
            }
            if !snapshot.metrics.is_empty() {
                let metrics = snapshot
                    .metrics
                    .iter()
                    .map(|m| format!("{}: {} {}", m.label, metric_text(m), m.unit))
                    .collect::<Vec<_>>()
                    .join(" · ");
                lines.push(Line::styled(clip(&format!(" {metrics}"), width), dim));
            }
            if let Some(detail) = snapshot.details.first() {
                lines.push(Line::styled(clip(&format!(" {detail}"), width), dim));
            }
        }
        if let Some(error) = &view.error {
            lines.push(Line::styled(
                clip(&format!(" stale: {error}"), width),
                Style::new().fg(Color::Yellow),
            ));
        }
    }
    let room = inner.height as usize;
    if lines.len() > room && room > 0 {
        lines.truncate(room);
        lines[room - 1] = Line::styled(" more tracks and details in /monitor", dim);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Snapshot;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn both_benchmark_tracks_are_visible_without_a_turn() {
        let snapshot: Snapshot = serde_json::from_str(r#"{"summary":"Benchmarking matrix multiply","tracks":[{"id":"newupdate","current":31,"total":65},{"id":"baseline","current":63,"total":65}]}"#).unwrap();
        let views = vec![View {
            id: "m1".into(),
            name: "Benchmark".into(),
            state: "running".into(),
            snapshot: Some(snapshot),
            error: None,
            updated_secs_ago: Some(1),
            detail: String::new(),
        }];
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, height(&views, 24)), &views, false))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("newupdate") && text.contains("31/65"),
            "{text}"
        );
        assert!(
            text.contains("baseline") && text.contains("63/65"),
            "{text}"
        );
        assert!(text.contains("updated 1s ago"), "{text}");
        assert_eq!(height(&[], 24), 0);
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 2, 1), &views, false))
            .unwrap();
    }
}
