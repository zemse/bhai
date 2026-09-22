//! Colour depth, and what to do when the terminal has less of it than a theme wants.
//! A syntect theme is written in 24-bit colour, which Apple's Terminal drops on the
//! floor rather than approximating, so every colour a theme asks for goes through here
//! and comes back as the nearest xterm-256 entry when truecolor is not on offer.

use std::sync::OnceLock;

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    True,
    Indexed,
}

static DEPTH: OnceLock<Depth> = OnceLock::new();

pub fn depth() -> Depth {
    *DEPTH.get_or_init(detect)
}

fn detect() -> Depth {
    // The terminal's own claim, and the only one worth taking at face value: a user who
    // sets it on a terminal that lies has said what they want.
    if let Ok(v) = std::env::var("COLORTERM") {
        let v = v.to_ascii_lowercase();
        if v == "truecolor" || v == "24bit" {
            return Depth::True;
        }
    }
    // `COLORTERM` does not survive ssh or a stripped environment, so the terminal is
    // named instead. Apple's Terminal is here to be ruled out: it advertises 256 and
    // has no truecolor to fall back on.
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        if program == "Apple_Terminal" {
            return Depth::Indexed;
        }
        if matches!(
            program.as_str(),
            "iTerm.app" | "vscode" | "WezTerm" | "ghostty" | "Hyper"
        ) {
            return Depth::True;
        }
    }
    if let Ok(term) = std::env::var("TERM")
        && (term.ends_with("-direct") || term.contains("truecolor") || term == "xterm-kitty")
    {
        return Depth::True;
    }
    Depth::Indexed
}

/// A theme's colour, as this terminal can show it.
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    match depth() {
        Depth::True => Color::Rgb(r, g, b),
        Depth::Indexed => Color::Indexed(nearest(r, g, b)),
    }
}

/// The nearest xterm-256 entry. Both the 6x6x6 cube and the 24-step grey ramp are
/// tried: a grey picked out of the cube alone lands up to 40 off, and code is mostly
/// greys.
fn nearest(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |c: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|&(_, &v)| (v as i32 - c as i32).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_dist = distance((r, g, b), (LEVELS[ri], LEVELS[gi], LEVELS[bi]));

    // The ramp is index 232 + n for grey 8 + 10n, n in 0..24.
    let average = (r as u16 + g as u16 + b as u16) / 3;
    let n = match average {
        ..8 => 0,
        239.. => 23,
        avg => ((avg - 8) / 10) as u8,
    };
    let grey = 8 + n * 10;
    if distance((r, g, b), (grey, grey, grey)) < cube_dist {
        232 + n
    } else {
        cube as u8
    }
}

fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let (dr, dg, db) = (
        a.0 as i32 - b.0 as i32,
        a.1 as i32 - b.1 as i32,
        a.2 as i32 - b.2 as i32,
    );
    dr * dr + dg * dg + db * db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corners_of_the_cube_are_exact() {
        assert_eq!(nearest(255, 255, 255), 231);
        assert_eq!(nearest(0, 0, 0), 16);
        assert_eq!(nearest(255, 0, 0), 196);
    }

    #[test]
    fn a_grey_comes_off_the_ramp_rather_than_the_cube() {
        // 128 sits 7 off the nearest ramp step and 42 off the nearest cube level, which
        // is the whole reason the ramp is consulted.
        let index = nearest(128, 128, 128);
        assert!((232..=255).contains(&index), "{index}");
        assert_eq!(nearest(8, 8, 8), 232);
    }

    #[test]
    fn a_colour_nearer_the_cube_stays_in_it() {
        assert!(!(232..=255).contains(&nearest(0, 175, 0)));
    }
}
