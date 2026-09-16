//! First-run splash: the Northsight summit filling the body while the
//! project tree is empty. Every cell is computed per frame — the mark's
//! nested chevrons drawn as line art that rises out of the ground on
//! fade-in, valley mist of value noise drifting at its feet, a glint
//! sweeping the outlines, a hashed starfield twinkling in the empty sky,
//! and the wordmark materializing in a carved-out band the scene never
//! paints. Indexed colors only: Terminal.app has no truecolor.
//!
//! The event loop ticks a repaint every [`FRAME`] while [`App::splash_active`]
//! holds; the scene itself is a pure function of elapsed time, so a missed
//! frame skips ahead instead of stuttering.

use crate::app::{App, Focus, HitTarget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::Duration;

/// Repaint cadence while the splash is up.
pub const FRAME: Duration = Duration::from_millis(100);

/// Seconds to fade the sky in from black. Also hides the splash flashing
/// briefly on every launch before the daemon's first tree snapshot lands.
const FADE_IN: f32 = 1.2;

/// The compact mark for panes too narrow for the block wordmark, and for
/// the empty-body hint: the summit glyph.
pub const MARK: &str = "▲ ";
/// The name under the mark.
pub const NAME: &str = "northsight";

/// Northsight greens on the xterm-256 cube: forest -> moss -> sage -> pale.
const GREENS: &[u8] = &[22, 28, 64, 71, 107, 150, 193];
/// The glint that sweeps the wordmark and the outlines.
const GLINT: u8 = 194;
/// Valley mist, thin and thick.
const MIST_THIN: u8 = 22;
const MIST_THICK: u8 = 65;

/// Columns per row of descent along a flank: steeper than 45° on a 2:1
/// terminal cell, like the mark.
const SLOPE: f32 = 1.6;

fn tone(g: f32) -> u8 {
    GREENS[(g.clamp(0.0, 1.0) * (GREENS.len() as f32 - 1.0)).round() as usize]
}

/// 5-row block bitmaps for N O R T H S I G H T.
const LETTERS: &[&[&str; 5]] = &[
    &["#...#", "##..#", "#.#.#", "#..##", "#...#"],
    &[".##.", "#..#", "#..#", "#..#", ".##."],
    &["###.", "#..#", "###.", "#.#.", "#..#"],
    &["#####", "..#..", "..#..", "..#..", "..#.."],
    &["#..#", "#..#", "####", "#..#", "#..#"],
    &[".###", "#...", ".##.", "...#", "###."],
    &["###", ".#.", ".#.", ".#.", "###"],
    &[".###", "#...", "#.##", "#..#", ".###"],
    &["#..#", "#..#", "####", "#..#", "#..#"],
    &["#####", "..#..", "..#..", "..#..", "..#.."],
];

fn wordmark_width() -> usize {
    LETTERS.iter().map(|l| l[0].len()).sum::<usize>() + 2 * (LETTERS.len() - 1)
}

fn hash(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(374_761_393)
        ^ (y as u32).wrapping_mul(668_265_263)
        ^ salt.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^ (h >> 16)
}

fn hash01(x: i32, y: i32, salt: u32) -> f32 {
    (hash(x, y, salt) & 0xffff) as f32 / 65535.0
}

/// One octave of smooth 2D value noise.
fn vnoise(x: f32, y: f32, salt: u32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let sx = fx * fx * (3.0 - 2.0 * fx);
    let sy = fy * fy * (3.0 - 2.0 * fy);
    let a = hash01(xi, yi, salt);
    let b = hash01(xi + 1, yi, salt);
    let c = hash01(xi, yi + 1, salt);
    let d = hash01(xi + 1, yi + 1, salt);
    a + (b - a) * sx + (c - a) * sy + (a - b - c + d) * sx * sy
}

/// Wordmark tone across the word: deep at the edges, sage in the middle,
/// mirroring the mark's light-centred gradient.
fn word_tone(u: f32) -> f32 {
    0.2 + 0.8 * (1.0 - (2.0 * u - 1.0).abs())
}

/// One wordmark row as per-cell spans: gradient across the word, a slow
/// glint sweeping through, and the blocks materializing from static
/// (`░` -> `▒` -> `█`) while the scene fades in.
fn wordmark_line(row: usize, t: f32, fade: f32) -> Line<'static> {
    let width = wordmark_width();
    let block = if fade < 0.5 {
        "░"
    } else if fade < 0.85 {
        "▒"
    } else {
        "█"
    };
    let mut spans = Vec::new();
    let mut col = 0usize;
    for (i, letter) in LETTERS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            col += 2;
        }
        for ch in letter[row].chars() {
            if ch == '#' {
                let u = col as f32 / width as f32;
                let shine = (u * 5.0 - t * 1.4).sin() > 0.93;
                let color = if shine && fade >= 1.0 {
                    GLINT
                } else {
                    tone(word_tone(u))
                };
                spans.push(Span::styled(
                    block,
                    Style::default()
                        .fg(Color::Indexed(color))
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(" "));
            }
            col += 1;
        }
    }
    Line::from(spans)
}

/// The rule the mark draws between the summit and the name, in the same
/// light-centred gradient; dashed until the wordmark has materialized.
fn rule_line(fade: f32) -> Line<'static> {
    let width = wordmark_width();
    let glyph = if fade < 0.85 { "╌" } else { "─" };
    let spans = (0..width)
        .map(|c| {
            let u = c as f32 / width as f32;
            Span::styled(
                glyph,
                Style::default().fg(Color::Indexed(tone(word_tone(u)))),
            )
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// One chevron of the mark: apex cell, tone at the feet, tone at the apex.
struct Chevron {
    ax: f32,
    ay: f32,
    lo: f32,
    hi: f32,
}

impl Chevron {
    /// Row the outline passes through at column `x`.
    fn row_at(&self, x: f32) -> f32 {
        self.ay + (x - self.ax).abs() / SLOPE
    }
}

/// Empty sky: sparse stars on their own twinkle phases, plus the rare
/// accent-colored sparkle.
fn star(buf: &mut Buffer, x: u16, y: u16, t: f32, fade: f32, accent: Color) {
    let h = hash(i32::from(x), i32::from(y), 12_345);
    if !h.is_multiple_of(53) {
        return;
    }
    let phase = ((h >> 8) % 8) as f32 * 0.8;
    let tw = ((t * 2.5 + phase).sin() * 0.5 + 0.5) * fade;
    if tw <= 0.45 {
        return;
    }
    if (h >> 4).is_multiple_of(111) {
        buf[(x, y)].set_char('+').set_fg(accent);
    } else if tw > 0.8 {
        buf[(x, y)].set_char('·').set_fg(Color::Indexed(189));
    } else {
        buf[(x, y)].set_char('.').set_fg(Color::Indexed(60));
    }
}

fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom()
}

/// The mark as line art in the sky above `carve`: the summit's outer and
/// inner chevrons and its small foot, the two shoulder peaks tucked behind
/// it wherever they pass under the summit, exactly as the logo nests them.
fn summit(buf: &mut Buffer, area: Rect, carve: Rect, t: f32, fade: f32, accent: Color) {
    let base = f32::from(carve.y) - 1.0;
    let top = f32::from(area.y) + 1.0;
    let h = (base - top).max(3.0);
    let cx = f32::from(area.x) + f32::from(area.width) / 2.0;
    let outer = Chevron {
        ax: cx,
        ay: base - h,
        lo: 0.4,
        hi: 0.85,
    };
    let inner = Chevron {
        ax: cx,
        ay: base - h * 0.70,
        lo: 0.65,
        hi: 1.0,
    };
    let foot = Chevron {
        ax: cx,
        ay: base - h * 0.36,
        lo: 0.45,
        hi: 0.7,
    };
    let left = Chevron {
        ax: cx - h * SLOPE * 0.62,
        ay: base - h * 0.62,
        lo: 0.0,
        hi: 0.35,
    };
    let right = Chevron {
        ax: cx + h * SLOPE * 0.68,
        ay: base - h * 0.68,
        lo: 0.0,
        hi: 0.35,
    };
    // Back to front: the summit paints over the shoulders where they meet.
    let order: [(&Chevron, bool); 5] = [
        (&left, true),
        (&right, true),
        (&outer, false),
        (&inner, false),
        (&foot, false),
    ];
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if in_rect(carve, x, y) {
                continue;
            }
            let (xf, yf) = (f32::from(x), f32::from(y));
            // The mark rises out of the ground while the scene fades in.
            let risen = yf <= base && (base - yf) <= fade * (h + 2.0);
            let mut hit: Option<(char, u8)> = None;
            if risen {
                for (c, shoulder) in order {
                    let r = c.row_at(xf);
                    if r > base || (yf - r).abs() > 0.75 {
                        continue;
                    }
                    if shoulder && yf >= outer.row_at(xf) - 0.75 {
                        continue; // hidden behind the summit
                    }
                    let glyph = if (xf - c.ax).abs() < 0.5 {
                        '▲'
                    } else if xf < c.ax {
                        '╱'
                    } else {
                        '╲'
                    };
                    let g = (base - yf) / (base - c.ay).max(1.0);
                    let p = (xf - c.ax) / (h * SLOPE);
                    let shine = (p * 3.0 - t * 1.1).sin() > 0.95;
                    let color = if shine && fade >= 1.0 {
                        GLINT
                    } else {
                        tone(c.lo + (c.hi - c.lo) * g)
                    };
                    hit = Some((glyph, color));
                }
            }
            if let Some((ch, color)) = hit {
                buf[(x, y)].set_char(ch).set_fg(Color::Indexed(color));
                continue;
            }
            // Valley mist hugging the feet, drifting slowly to the right.
            if yf <= base && yf > base - h * 0.55 {
                let m = vnoise(xf * 0.09 - t * 0.12, yf * 0.35, 4242)
                    * (-(base - yf) / (h * 0.28)).exp()
                    * fade;
                if m > 0.30 {
                    let (ch, color) = if m > 0.5 {
                        (':', MIST_THICK)
                    } else {
                        ('.', MIST_THIN)
                    };
                    buf[(x, y)].set_char(ch).set_fg(Color::Indexed(color));
                    continue;
                }
            }
            star(buf, x, y, t, fade, accent);
        }
    }
}

pub fn draw_splash(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    if area.width < 8 || area.height < 4 {
        return;
    }
    // Animations off: the event loop doesn't tick us, so hold one finished
    // frame (well past the fade-in) instead of whatever instant a stray
    // redraw lands on.
    let t = if app.animations {
        app.splash_epoch.elapsed().as_secs_f32()
    } else {
        60.0
    };
    let raw = (t / FADE_IN).clamp(0.0, 1.0);
    let fade = raw * raw * (3.0 - 2.0 * raw);

    // ---- text block: wordmark, rule, tagline, key hints, bottom-anchored ----
    let big = area.width >= wordmark_width() as u16 + 6 && area.height >= 20;
    let mut lines: Vec<Line> = Vec::new();
    if big {
        for row in 0..5 {
            lines.push(wordmark_line(row, t, fade));
        }
        lines.push(Line::from(""));
        lines.push(rule_line(fade));
    } else {
        lines.push(Line::from(vec![
            Span::styled(MARK, Style::default().fg(th.accent)),
            Span::styled(
                NAME,
                Style::default().fg(th.text).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    if area.width >= 47 {
        lines.push(Line::from(Span::styled(
            "your agents keep running, even when you leave",
            Style::default().fg(th.dim),
        )));
        lines.push(Line::from(""));
    }
    let key = |k: &str, label: &str| {
        vec![
            Span::styled(
                k.to_string(),
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {label}"), Style::default().fg(th.dim)),
        ]
    };
    let mut hint = Vec::new();
    if !app.tree.has_visible_projects() {
        hint.extend(key("n / o", "create your first project"));
        hint.push(Span::styled("   ·   ", Style::default().fg(th.dim)));
        hint.extend(key("?", "help"));
    } else {
        // Summoned as a preview over a populated tree.
        hint.extend(key("any key", "returns"));
    }
    lines.push(Line::from(hint));

    let block_w = (lines.iter().map(Line::width).max().unwrap_or(0) as u16).min(area.width);
    let block_h = (lines.len() as u16).min(area.height);
    let text = Rect {
        x: area.x + (area.width - block_w) / 2,
        y: area.y + area.height - block_h - u16::from(area.height > block_h),
        width: block_w,
        height: block_h,
    };
    // Text carve: rows the scene never touches.
    let carve = Rect {
        x: text.x.saturating_sub(3),
        y: text.y.saturating_sub(1),
        width: text.width + 6,
        height: text.height + 2,
    }
    .intersection(area);

    summit(f.buffer_mut(), area, carve, t, fade, th.accent);

    f.render_widget(Paragraph::new(lines).centered(), text);
    // A click anywhere lands focus back on the (invisible) projects panel,
    // where `n` creates the first project.
    app.hits.push((area, HitTarget::PanelBg(Focus::Projects)));
}
