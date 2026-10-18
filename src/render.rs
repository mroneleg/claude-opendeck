//! Key images rendered as SVG data URIs (OpenDeck rasterises them with resvg).

use base64_encode::encode as base64;

use crate::activity::Totals;
use crate::sessions::Status;
use crate::usage::{BreakdownRow, Spend, Usage, Window, expected_percent};

const SIZE: u32 = 144;

pub const BG: &str = "#18181b";
pub const FG: &str = "#fafafa";
pub const MUTED: &str = "#a1a1aa";
pub const TRACK: &str = "#3f3f46";
pub const CLAUDE: &str = "#d97757";
pub const CLAUDE_DARK: &str = "#b85a3b";
pub const WARN: &str = "#f59e0b";
pub const DANGER: &str = "#ef4444";
pub const WORKING: &str = "#2563eb";
pub const DONE: &str = "#16a34a";
/// Text drawn on the accent colour; `theme` swaps it for a contrasting colour.
pub const ON_ACCENT: &str = "#fffffe";
/// Within this many points of the even-spend line counts as "on pace".
const PACE_TOLERANCE: f64 = 5.0;

mod base64_encode {
    /// Standard base64 with padding (small enough not to need a crate).
    pub fn encode(input: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A complete 144x144 SVG document with a solid background.
pub fn svg(body: &str, background: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}"><rect width="{SIZE}" height="{SIZE}" fill="{background}"/>{body}</svg>"#
    )
}

pub fn data_uri(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", base64(svg.as_bytes()))
}

fn image(body: &str) -> String {
    data_uri(&svg(body, BG))
}

fn image_on(body: &str, background: &str) -> String {
    data_uri(&svg(body, background))
}

fn color_for(percent: f64) -> &'static str {
    if percent >= 90.0 {
        DANGER
    } else if percent >= 70.0 {
        WARN
    } else {
        CLAUDE
    }
}

#[derive(Clone, Copy)]
struct Text<'a> {
    size: u32,
    color: &'a str,
    bold: bool,
    anchor: &'a str,
}

const TEXT: Text<'static> = Text {
    size: 14,
    color: FG,
    bold: false,
    anchor: "middle",
};

fn text(x: f64, y: f64, value: &str, style: Text<'_>) -> String {
    format!(
        r#"<text x="{x}" y="{y}" fill="{}" font-family="sans-serif" font-size="{}" font-weight="{}" text-anchor="{}">{}</text>"#,
        style.color,
        style.size,
        if style.bold { "bold" } else { "normal" },
        style.anchor,
        escape_xml(value)
    )
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() > max {
        let kept: String = value.chars().take(max - 1).collect();
        format!("{kept}…")
    } else {
        value.to_owned()
    }
}

/// Progress bar; `expected` (0-100) draws a tick where even spending would be.
fn bar(x: f64, y: f64, width: f64, height: f64, percent: f64, expected: Option<f64>, color: Option<&str>) -> String {
    let p = percent.clamp(0.0, 100.0);
    let color = color.unwrap_or_else(|| color_for(percent));
    let radius = height / 2.0;
    let mut out = format!(r#"<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="{radius}" fill="{TRACK}"/>"#);
    if p > 0.0 {
        let filled = (width * p / 100.0).max(height);
        out +=
            &format!(r#"<rect x="{x}" y="{y}" width="{filled:.1}" height="{height}" rx="{radius}" fill="{color}"/>"#);
    }
    if let Some(expected) = expected {
        let tick = x + width * expected / 100.0 - 1.5;
        out += &format!(
            r#"<rect x="{tick:.1}" y="{}" width="3" height="{}" rx="1.5" fill="{FG}"/>"#,
            y - 3.0,
            height + 6.0
        );
    }
    out
}

/// "2h 13m", "3d 4h", "45m" until `timestamp`.
pub fn format_countdown(timestamp: Option<i64>, now: i64) -> String {
    let Some(timestamp) = timestamp else {
        return String::new();
    };
    let minutes = (((timestamp - now) as f64) / 60000.0).round().max(0.0) as i64;
    if minutes >= 24 * 60 {
        format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60)
    } else if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn pct(percent: f64) -> String {
    format!("{}%", percent.round() as i64)
}

/// (label, color, expected%) comparing usage with the even-spend line.
pub fn pace(window: &Window, now: i64) -> Option<(String, &'static str, f64)> {
    let expected = expected_percent(window, now)?;
    let delta = (window.percent - expected).round();
    Some(if delta.abs() <= PACE_TOLERANCE {
        ("on pace".into(), MUTED, expected)
    } else if delta > 0.0 {
        (format!("+{delta}% pace"), WARN, expected)
    } else {
        (format!("{delta}% pace"), MUTED, expected)
    })
}

fn both_image(session: Option<&Window>, weekly: Option<&Window>, now: i64) -> String {
    let rows: String = [session, weekly]
        .iter()
        .enumerate()
        .filter_map(|(i, w)| w.map(|w| (i, w)))
        .map(|(i, w)| {
            let top = 14.0 + i as f64 * 64.0;
            let pace = pace(w, now);
            let mut row = text(
                12.0,
                top + 22.0,
                &w.label,
                Text {
                    size: 18,
                    color: MUTED,
                    bold: true,
                    anchor: "start",
                },
            ) + &text(
                132.0,
                top + 22.0,
                &pct(w.percent),
                Text {
                    size: 26,
                    bold: true,
                    anchor: "end",
                    ..TEXT
                },
            ) + &bar(
                12.0,
                top + 32.0,
                120.0,
                9.0,
                w.percent,
                pace.as_ref().map(|p| p.2),
                None,
            );
            if let Some((label, color, _)) = &pace {
                row += &text(
                    12.0,
                    top + 58.0,
                    label,
                    Text {
                        size: 12,
                        color,
                        anchor: "start",
                        ..TEXT
                    },
                );
            }
            row + &text(
                132.0,
                top + 58.0,
                &format_countdown(w.resets_at, now),
                Text {
                    size: 12,
                    color: MUTED,
                    anchor: "end",
                    ..TEXT
                },
            )
        })
        .collect();
    image(&rows)
}

fn single_image(window: Option<&Window>, title: &str, now: i64) -> String {
    let Some(w) = window else {
        return message_image(title, "n/a", MUTED);
    };
    let pace = pace(w, now);
    let mut body = text(
        72.0,
        26.0,
        &truncate(title, 14),
        Text {
            size: 16,
            color: MUTED,
            bold: true,
            ..TEXT
        },
    ) + &text(
        72.0,
        72.0,
        &pct(w.percent),
        Text {
            size: 40,
            bold: true,
            color: if w.percent >= 70.0 { color_for(w.percent) } else { FG },
            ..TEXT
        },
    ) + &bar(14.0, 88.0, 116.0, 10.0, w.percent, pace.as_ref().map(|p| p.2), None);
    if let Some((label, color, _)) = &pace {
        body += &text(
            72.0,
            118.0,
            label,
            Text {
                size: 13,
                color,
                ..TEXT
            },
        );
    }
    body += &text(
        72.0,
        136.0,
        &format!("resets {}", format_countdown(w.resets_at, now)),
        Text {
            size: 13,
            color: MUTED,
            ..TEXT
        },
    );
    image(&body)
}

pub fn message_image(line1: &str, line2: &str, color: &str) -> String {
    image(
        &(text(
            72.0,
            66.0,
            line1,
            Text {
                size: 18,
                bold: true,
                ..TEXT
            },
        ) + &text(
            72.0,
            92.0,
            line2,
            Text {
                size: 14,
                color,
                ..TEXT
            },
        )),
    )
}

pub fn usage_image(usage: &Usage, mode: &str, now: i64) -> String {
    match mode {
        "session" => single_image(usage.session.as_ref(), "Session", now),
        "weekly" => single_image(usage.weekly.as_ref(), "Weekly", now),
        "model" => match usage.scoped.first() {
            Some(top) => single_image(Some(top), &format!("{} weekly", top.label), now),
            None => message_image("Per-model", "no model limits", MUTED),
        },
        _ => both_image(usage.session.as_ref(), usage.weekly.as_ref(), now),
    }
}

/// Flash frame for a crossed threshold.
pub fn alert_image(label: &str, percent: f64) -> String {
    let background = if color_for(percent) == CLAUDE {
        WARN
    } else {
        color_for(percent)
    };
    image_on(
        &(text(
            72.0,
            56.0,
            "!",
            Text {
                size: 44,
                bold: true,
                color: "#fff",
                ..TEXT
            },
        ) + &text(
            72.0,
            96.0,
            &pct(percent),
            Text {
                size: 30,
                bold: true,
                color: "#fff",
                ..TEXT
            },
        ) + &text(
            72.0,
            124.0,
            &truncate(label, 14),
            Text {
                size: 15,
                color: "#fff",
                ..TEXT
            },
        )),
        background,
    )
}

/// Share of this week's usage by surface (Claude Code, Chats, ...).
pub fn breakdown_image(rows: &[BreakdownRow]) -> String {
    if rows.is_empty() {
        return message_image("Breakdown", "no data", MUTED);
    }
    let body: String = rows
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, row)| {
            let y = 46.0 + i as f64 * 25.0;
            text(
                10.0,
                y,
                &truncate(&row.name, 12),
                Text {
                    size: 13,
                    anchor: "start",
                    ..TEXT
                },
            ) + &text(
                134.0,
                y,
                &pct(row.percent),
                Text {
                    size: 13,
                    color: MUTED,
                    anchor: "end",
                    ..TEXT
                },
            ) + &bar(10.0, y + 5.0, 124.0, 5.0, row.percent, None, Some(CLAUDE))
        })
        .collect();
    // Heading low enough to clear a status badge in the top corner.
    image(
        &(text(
            72.0,
            25.0,
            "Week by surface",
            Text {
                size: 13,
                color: MUTED,
                bold: true,
                ..TEXT
            },
        ) + &body),
    )
}

fn format_money(amount: f64, currency: &str) -> String {
    if currency == "USD" {
        format!("${amount:.2}")
    } else {
        format!("{amount:.2} {currency}")
    }
}

/// Extra usage (credits beyond the plan): spent so far, limit, enabled state.
pub fn spend_image(spend: Option<&Spend>) -> String {
    let Some(spend) = spend else {
        return message_image("Extra usage", "n/a", MUTED);
    };
    if !spend.enabled {
        let reason = spend
            .disabled_reason
            .as_deref()
            .map_or_else(|| "off".to_owned(), |r| truncate(r, 18));
        return message_image("Extra usage", &reason, MUTED);
    }
    let percent = spend.percent.unwrap_or(match spend.limit {
        Some(limit) if limit > 0.0 => spend.used / limit * 100.0,
        _ => 0.0,
    });
    let mut body = text(
        72.0,
        28.0,
        "Extra usage",
        Text {
            size: 15,
            color: MUTED,
            bold: true,
            ..TEXT
        },
    ) + &text(
        72.0,
        78.0,
        &format_money(spend.used, &spend.currency),
        Text {
            size: if spend.used >= 1000.0 { 26 } else { 32 },
            bold: true,
            color: if spend.used > 0.0 { CLAUDE } else { FG },
            ..TEXT
        },
    );
    if spend.limit.is_some() {
        body += &bar(14.0, 94.0, 116.0, 9.0, percent, None, None);
    }
    let footer = spend.limit.map_or_else(
        || "no limit set".to_owned(),
        |l| format!("of {}", format_money(l, &spend.currency)),
    );
    body += &text(
        72.0,
        128.0,
        &footer,
        Text {
            size: 13,
            color: MUTED,
            ..TEXT
        },
    );
    image(&body)
}

/// Lights up in Claude orange when any session is waiting on you.
pub fn needs_you_image(waiting: usize, latest_project: Option<&str>, working: usize) -> String {
    if waiting > 0 {
        let heading = if waiting == 1 {
            "Claude needs you".to_owned()
        } else {
            format!("{waiting} need you")
        };
        return image_on(
            &(text(
                72.0,
                30.0,
                &heading,
                Text {
                    size: 15,
                    color: ON_ACCENT,
                    bold: true,
                    ..TEXT
                },
            ) + &format!(r#"<circle cx="72" cy="66" r="22" fill="{ON_ACCENT}"/>"#)
                + &text(
                    72.0,
                    77.0,
                    "!",
                    Text {
                        size: 32,
                        bold: true,
                        color: CLAUDE,
                        ..TEXT
                    },
                )
                + &text(
                    72.0,
                    112.0,
                    &truncate(latest_project.unwrap_or(""), 16),
                    Text {
                        size: 15,
                        color: ON_ACCENT,
                        bold: true,
                        ..TEXT
                    },
                )
                + &text(
                    72.0,
                    132.0,
                    "press to open",
                    Text {
                        size: 12,
                        color: ON_ACCENT,
                        ..TEXT
                    },
                )),
            CLAUDE,
        );
    }
    all_clear_image(working, 0)
}

/// Number of running Claude Code sessions.
pub fn sessions_image(count: usize, busy: usize) -> String {
    let footer = if busy > 0 {
        format!("{busy} busy")
    } else if count > 0 {
        "all idle".into()
    } else {
        "none running".into()
    };
    image(
        &(text(
            72.0,
            28.0,
            "Sessions",
            Text {
                size: 15,
                color: MUTED,
                bold: true,
                ..TEXT
            },
        ) + &text(
            72.0,
            86.0,
            &count.to_string(),
            Text {
                size: 48,
                bold: true,
                color: if count > 0 { FG } else { MUTED },
                ..TEXT
            },
        ) + &text(
            72.0,
            124.0,
            &footer,
            Text {
                size: 14,
                color: if busy > 0 { CLAUDE } else { MUTED },
                ..TEXT
            },
        )),
    )
}

/// 1234 -> "1.2k", 32327629 -> "32.3M".
pub fn format_tokens(n: u64) -> String {
    let n_f = n as f64;
    if n_f >= 1e9 {
        format!("{:.1}B", n_f / 1e9)
    } else if n_f >= 1e6 {
        format!("{:.1}M", n_f / 1e6)
    } else if n_f >= 1e3 {
        format!("{:.1}k", n_f / 1e3)
    } else {
        n.to_string()
    }
}

/// Today's token totals from local transcripts.
pub fn activity_image(t: &Totals) -> String {
    let small = Text {
        size: 13,
        color: MUTED,
        anchor: "start",
        ..TEXT
    };
    let value = Text {
        size: 13,
        anchor: "end",
        ..TEXT
    };
    image(
        &(text(
            72.0,
            24.0,
            "Today",
            Text {
                size: 15,
                color: MUTED,
                bold: true,
                ..TEXT
            },
        ) + &text(
            72.0,
            64.0,
            &format_tokens(t.total()),
            Text {
                size: 32,
                bold: true,
                ..TEXT
            },
        ) + &text(
            72.0,
            80.0,
            "tokens",
            Text {
                size: 12,
                color: MUTED,
                ..TEXT
            },
        ) + &text(10.0, 104.0, "out", small)
            + &text(134.0, 104.0, &format_tokens(t.output), value)
            + &text(10.0, 122.0, "in", small)
            + &text(134.0, 122.0, &format_tokens(t.input + t.cache_create), value)
            + &text(10.0, 140.0, "cached", small)
            + &text(134.0, 140.0, &format_tokens(t.cache_read), value)),
    )
}

/// Terminal prompt on Claude orange with the project name.
pub fn launch_image(project: &str) -> String {
    let name = if project.is_empty() { "Claude" } else { project };
    image_on(
        &(format!(r#"<rect x="24" y="22" width="96" height="66" rx="10" fill="{BG}"/>"#)
            + &format!(
                r#"<path d="M40 42 L54 55 L40 68" fill="none" stroke="{CLAUDE}" stroke-width="7" stroke-linecap="round" stroke-linejoin="round"/>"#
            )
            + &format!(r#"<rect x="62" y="64" width="34" height="7" rx="3.5" fill="{FG}"/>"#)
            + &text(
                72.0,
                116.0,
                &truncate(name, 14),
                Text {
                    size: 15,
                    color: ON_ACCENT,
                    bold: true,
                    ..TEXT
                },
            )
            + &text(
                72.0,
                134.0,
                "launch",
                Text {
                    size: 12,
                    color: ON_ACCENT,
                    ..TEXT
                },
            )),
        CLAUDE,
    )
}

/// "just now", "4m ago", "2h ago", "3d ago".
pub fn format_age(age_ms: i64) -> String {
    let minutes = age_ms.max(0) / 60_000;
    if minutes < 1 {
        "just now".into()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 48 * 60 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / 1440)
    }
}

/// Lit session-status key for working (blue) or done (green) sessions.
/// Waiting uses [`needs_you_image`]; nothing lit uses [`all_clear_image`].
pub fn status_image(status: Status, count: usize, project: Option<&str>) -> String {
    let (background, heading, footer, glyph) = match status {
        Status::Done => (
            DONE,
            if count == 1 {
                "Done".to_owned()
            } else {
                format!("{count} done")
            },
            "press to clear",
            r##"<path d="M58 66 L68 76 L88 56" fill="none" stroke="#ffffff" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/>"##,
        ),
        _ => (
            WORKING,
            if count == 1 {
                "Working".to_owned()
            } else {
                format!("{count} working")
            },
            "hold to open",
            r##"<circle cx="58" cy="66" r="5" fill="#ffffff"/><circle cx="72" cy="66" r="5" fill="#ffffff" fill-opacity="0.75"/><circle cx="86" cy="66" r="5" fill="#ffffff" fill-opacity="0.5"/>"##,
        ),
    };
    let white = Text {
        color: "#ffffff",
        ..TEXT
    };
    image_on(
        &(text(
            72.0,
            30.0,
            &heading,
            Text {
                size: 15,
                bold: true,
                ..white
            },
        ) + r##"<circle cx="72" cy="66" r="22" fill="none" stroke="#ffffff" stroke-opacity="0.5" stroke-width="3"/>"##
            + glyph
            + &text(
                72.0,
                112.0,
                &truncate(project.unwrap_or(""), 16),
                Text {
                    size: 15,
                    bold: true,
                    ..white
                },
            )
            + &text(72.0, 132.0, footer, Text { size: 12, ..white })),
        background,
    )
}

/// Nothing lights the key: a check mark and counts of what else is going on.
pub fn all_clear_image(working: usize, done: usize) -> String {
    let footer = match (working, done) {
        (0, 0) => String::new(),
        (w, 0) => format!("{w} working"),
        (0, d) => format!("{d} done"),
        (w, d) => format!("{w} working · {d} done"),
    };
    image(
        &(text(
            72.0,
            30.0,
            "Claude",
            Text {
                size: 15,
                color: MUTED,
                bold: true,
                ..TEXT
            },
        ) + &format!(r#"<circle cx="72" cy="68" r="22" fill="none" stroke="{TRACK}" stroke-width="5"/>"#)
            + &format!(
                r#"<path d="M61 68 L69 76 L84 60" fill="none" stroke="{MUTED}" stroke-width="5" stroke-linecap="round" stroke-linejoin="round"/>"#
            )
            + &text(
                72.0,
                116.0,
                "All clear",
                Text {
                    size: 15,
                    bold: true,
                    ..TEXT
                },
            )
            + &text(
                72.0,
                134.0,
                &footer,
                Text {
                    size: 12,
                    color: MUTED,
                    ..TEXT
                },
            )),
    )
}

// --- dial (encoder) touch strip: 200x100, OpenDeck's "$A0" full-canvas layout ------

const STRIP_W: u32 = 200;
const STRIP_H: u32 = 100;

fn strip(body: &str, background: &str) -> String {
    data_uri(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{STRIP_W}" height="{STRIP_H}" viewBox="0 0 {STRIP_W} {STRIP_H}"><rect width="{STRIP_W}" height="{STRIP_H}" fill="{background}"/>{body}</svg>"#
    ))
}

fn strip_row(window: &Window, y: f64, now: i64) -> String {
    let pace = pace(window, now);
    text(
        12.0,
        y,
        &window.label,
        Text {
            size: 16,
            color: MUTED,
            bold: true,
            anchor: "start",
        },
    ) + &text(
        188.0,
        y,
        &pct(window.percent),
        Text {
            size: 20,
            bold: true,
            anchor: "end",
            ..TEXT
        },
    ) + &bar(12.0, y + 7.0, 176.0, 7.0, window.percent, pace.map(|p| p.2), None)
}

/// Usage on a dial's touch strip, in the key's display mode.
pub fn usage_strip(usage: &Usage, mode: &str, now: i64) -> String {
    let single = |window: Option<&Window>, title: &str| match window {
        None => strip(
            &text(100.0, 56.0, &format!("{title}: n/a"), Text { color: MUTED, ..TEXT }),
            BG,
        ),
        Some(w) => {
            let mut body = text(
                12.0,
                26.0,
                &truncate(title, 16),
                Text {
                    size: 15,
                    color: MUTED,
                    bold: true,
                    anchor: "start",
                },
            ) + &text(
                188.0,
                58.0,
                &pct(w.percent),
                Text {
                    size: 34,
                    bold: true,
                    anchor: "end",
                    color: if w.percent >= 70.0 { color_for(w.percent) } else { FG },
                },
            ) + &bar(12.0, 70.0, 176.0, 9.0, w.percent, pace(w, now).map(|p| p.2), None);
            body += &text(
                12.0,
                94.0,
                &format!("resets {}", format_countdown(w.resets_at, now)),
                Text {
                    size: 12,
                    color: MUTED,
                    anchor: "start",
                    ..TEXT
                },
            );
            if let Some((label, color, _)) = pace(w, now) {
                body += &text(
                    188.0,
                    94.0,
                    &label,
                    Text {
                        size: 12,
                        color,
                        anchor: "end",
                        ..TEXT
                    },
                );
            }
            strip(&body, BG)
        }
    };
    match mode {
        "session" => single(usage.session.as_ref(), "Session"),
        "weekly" => single(usage.weekly.as_ref(), "Weekly"),
        "model" => match usage.scoped.first() {
            Some(top) => single(Some(top), &format!("{} weekly", top.label)),
            None => single(None, "Per-model"),
        },
        _ => {
            let rows: String = [usage.session.as_ref(), usage.weekly.as_ref()]
                .iter()
                .enumerate()
                .filter_map(|(i, w)| w.map(|w| strip_row(w, 34.0 + i as f64 * 40.0, now)))
                .collect();
            strip(&rows, BG)
        }
    }
}

/// A message on a dial's touch strip (errors, "needs API", ...).
pub fn message_strip(line1: &str, line2: &str, color: &str) -> String {
    strip(
        &(text(
            100.0,
            46.0,
            line1,
            Text {
                size: 18,
                bold: true,
                ..TEXT
            },
        ) + &text(100.0, 72.0, line2, Text { color, ..TEXT })),
        BG,
    )
}

/// One session on a dial's touch strip: its status, project, and position.
pub fn session_strip(status: Status, project: &str, index: usize, total: usize) -> String {
    let (background, color) = match status {
        Status::Waiting => (CLAUDE, ON_ACCENT),
        Status::Working => (WORKING, "#ffffff"),
        Status::Done => (DONE, "#ffffff"),
        Status::Idle => (BG, FG),
    };
    let muted = if status == Status::Idle { MUTED } else { color };
    strip(
        &(text(
            12.0,
            28.0,
            &format!("{} / {total}", index + 1),
            Text {
                size: 13,
                color: muted,
                anchor: "start",
                ..TEXT
            },
        ) + &text(
            188.0,
            28.0,
            status.label(),
            Text {
                size: 14,
                color,
                bold: true,
                anchor: "end",
            },
        ) + &text(
            100.0,
            70.0,
            &truncate(project, 18),
            Text {
                size: 22,
                color,
                bold: true,
                ..TEXT
            },
        )),
        background,
    )
}

/// The usage gauge drawn on the plugin and category icons.
fn gauge(color: &str) -> String {
    format!(
        r#"<path d="M28 96 A44 44 0 0 1 116 96" fill="none" stroke="{color}" stroke-opacity="0.35" stroke-width="12" stroke-linecap="round"/><path d="M28 96 A44 44 0 0 1 94 58" fill="none" stroke="{color}" stroke-width="12" stroke-linecap="round"/><path d="M72 96 L96 64" stroke="{color}" stroke-width="7" stroke-linecap="round"/><circle cx="72" cy="96" r="9" fill="{color}"/><rect x="40" y="112" width="64" height="8" rx="4" fill="{color}" fill-opacity="0.6"/>"#
    )
}

/// Plugin icon: white gauge on a rounded Claude-orange tile (transparent corners).
pub fn plugin_icon_svg() -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{CLAUDE}"/><stop offset="1" stop-color="{CLAUDE_DARK}"/></linearGradient></defs><rect width="{SIZE}" height="{SIZE}" rx="28" fill="url(#g)"/>{}</svg>"#,
        gauge("#ffffff")
    )
}

/// Category icon: the white gauge on transparent.
pub fn category_icon_svg() -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">{}</svg>"#,
        gauge("#ffffff")
    )
}

/// Decodes one of our data URIs back to SVG text (for the icon builder and tests).
pub fn svg_from_data_uri(uri: &str) -> Option<String> {
    let encoded = uri.strip_prefix("data:image/svg+xml;base64,")?;
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in encoded.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(v);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for input in ["", "a", "ab", "abc", "abcd", "<svg>é…</svg>"] {
            let uri = data_uri(input);
            assert_eq!(svg_from_data_uri(&uri).as_deref(), Some(input));
        }
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
    }

    #[test]
    fn countdown_and_tokens() {
        assert_eq!(format_countdown(Some(60_000 * 133), 0), "2h 13m");
        assert_eq!(format_countdown(Some(60_000 * (3 * 1440 + 4 * 60)), 0), "3d 4h");
        assert_eq!(format_countdown(Some(-5), 0), "0m");
        assert_eq!(format_countdown(None, 0), "");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(32_327_629), "32.3M");
    }

    #[test]
    fn pace_labels() {
        let w = |percent| Window {
            label: "7d".into(),
            percent,
            resets_at: Some(50),
            window_ms: 100,
        };
        // Half the window elapsed at now=0.
        assert_eq!(pace(&w(52.0), 0).unwrap().0, "on pace");
        assert_eq!(pace(&w(70.0), 0).unwrap().0, "+20% pace");
        assert_eq!(pace(&w(10.0), 0).unwrap().0, "-40% pace");
    }

    #[test]
    fn escapes_and_truncates() {
        assert_eq!(escape_xml("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
        assert_eq!(truncate("managed-services-platform", 16), "managed-service…");
        let svg = svg_from_data_uri(&needs_you_image(2, Some("x<y"), 0)).unwrap();
        assert!(svg.contains("2 need you") && svg.contains("x&lt;y"));
        assert_eq!(format_age(30_000), "just now");
        assert_eq!(format_age(16 * 60_000), "16m ago");
        assert_eq!(format_age(3 * 3_600_000), "3h ago");
        assert_eq!(format_age(3 * 86_400_000), "3d ago");
    }
}
