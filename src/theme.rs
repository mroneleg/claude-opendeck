//! Per-key colours. Key images are drawn in the default palette (see
//! `render`), then recoloured here in one pass, so a chosen colour can never
//! be caught by a later replacement. Text on the background and on the accent
//! switches between light and dark for contrast.

use crate::render::{self, BG, CLAUDE, FG, MUTED, ON_ACCENT, TRACK};

const DARK_TEXT: &str = "#18181b";
const LIGHT_TEXT: &str = "#fafafa";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub background: String,
    pub accent: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: BG.into(),
            accent: CLAUDE.into(),
        }
    }
}

/// Strict `#rrggbb` (anything else could inject markup into the SVG); lowercased.
pub fn parse_color(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("#{}", hex.to_ascii_lowercase()))
}

fn channels(color: &str) -> [f64; 3] {
    let hex = color.trim_start_matches('#');
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_or(0.0, f64::from) / 255.0;
    [channel(0), channel(2), channel(4)]
}

/// WCAG relative luminance of a `#rrggbb` colour.
pub fn luminance(color: &str) -> f64 {
    let linear = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let [r, g, b] = channels(color).map(linear);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Light or dark text, whichever contrasts more with `background`.
pub fn text_on(background: &str) -> &'static str {
    if contrast(background, LIGHT_TEXT) >= contrast(background, DARK_TEXT) {
        LIGHT_TEXT
    } else {
        DARK_TEXT
    }
}

impl Theme {
    /// From a key's settings; invalid colours fall back to the defaults.
    pub fn from_settings(background: &str, accent: &str) -> Self {
        let default = Theme::default();
        Theme {
            background: parse_color(background).unwrap_or(default.background),
            accent: parse_color(accent).unwrap_or(default.accent),
        }
    }

    fn is_default(&self) -> bool {
        *self == Theme::default()
    }

    fn palette(&self) -> [(&'static str, String); 6] {
        let light = text_on(&self.background) == DARK_TEXT;
        [
            (BG, self.background.clone()),
            (FG, text_on(&self.background).into()),
            (MUTED, (if light { "#52525b" } else { MUTED }).into()),
            (TRACK, (if light { "#d4d4d8" } else { TRACK }).into()),
            (CLAUDE, self.accent.clone()),
            // White on the default orange is the original design; keep it.
            (
                ON_ACCENT,
                (if self.accent == CLAUDE {
                    "#ffffff"
                } else {
                    text_on(&self.accent)
                })
                .into(),
            ),
        ]
    }

    /// Recolours an SVG drawn in the default palette, in a single pass.
    pub fn recolor(&self, svg: &str) -> String {
        let palette = self.palette();
        let mut out = String::with_capacity(svg.len());
        let mut rest = svg;
        while let Some(index) = rest.find('#') {
            out.push_str(&rest[..index]);
            let candidate = rest.get(index..index + 7).unwrap_or("");
            match palette.iter().find(|(token, _)| *token == candidate) {
                Some((_, color)) => {
                    out.push_str(color);
                    rest = &rest[index + 7..];
                }
                None => {
                    out.push('#');
                    rest = &rest[index + 1..];
                }
            }
        }
        out.push_str(rest);
        out
    }
}

/// A small status mark in the top-right corner: a coloured dot and short text.
#[derive(Debug, Clone, PartialEq)]
pub struct Badge {
    pub color: &'static str,
    pub text: String,
}

fn badge_svg(badge: &Badge, width: f64) -> String {
    format!(
        r#"<circle cx="{}" cy="8" r="4.5" fill="{}"/><text x="{}" y="12" fill="{MUTED}" font-family="sans-serif" font-size="11" text-anchor="end">{}</text>"#,
        width - 8.0,
        badge.color,
        width - 16.0,
        render::escape_xml(&badge.text)
    )
}

/// Applies the theme and an optional badge to one of our image data URIs.
pub fn finish(uri: &str, theme: &Theme, badge: Option<&Badge>) -> String {
    if theme.is_default() && badge.is_none() {
        return uri.to_owned();
    }
    let Some(mut svg) = render::svg_from_data_uri(uri) else {
        return uri.to_owned();
    };
    if let Some(badge) = badge {
        let width = svg
            .split_once("width=\"")
            .and_then(|(_, rest)| rest.split('"').next())
            .and_then(|w| w.parse().ok())
            .unwrap_or(144.0);
        if let Some(end) = svg.rfind("</svg>") {
            svg.insert_str(end, &badge_svg(badge, width));
        }
    }
    render::data_uri(&theme.recolor(&svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_validated_strictly() {
        assert_eq!(parse_color("#AbC123").as_deref(), Some("#abc123"));
        assert_eq!(parse_color(" #000000 ").as_deref(), Some("#000000"));
        for bad in [
            "abc123",
            "#abc",
            "#abcdefg",
            "#12345g",
            "red",
            "#123456\"/><script>",
            "",
        ] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        let theme = Theme::from_settings("#zzz", "nope");
        assert_eq!(theme, Theme::default());
    }

    #[test]
    fn text_contrasts_with_background() {
        assert_eq!(text_on("#000000"), LIGHT_TEXT);
        assert_eq!(text_on(BG), LIGHT_TEXT);
        assert_eq!(text_on("#ffffff"), DARK_TEXT);
        assert_eq!(text_on("#facc15"), DARK_TEXT); // yellow
        assert_eq!(text_on("#1d4ed8"), LIGHT_TEXT); // blue
        assert!(contrast(LIGHT_TEXT, "#000000") > 15.0);
    }

    #[test]
    fn recolors_in_one_pass() {
        // A light background equal to the default FG must not then be turned
        // into the (now dark) text colour.
        let theme = Theme::from_settings("#fafafa", "#2563eb");
        let svg = format!(
            r##"<rect fill="{BG}"/><text fill="{FG}">x</text><rect fill="{CLAUDE}"/><text fill="{ON_ACCENT}"/><b fill="#ef4444"/>"##
        );
        let out = theme.recolor(&svg);
        assert_eq!(
            out,
            r##"<rect fill="#fafafa"/><text fill="#18181b">x</text><rect fill="#2563eb"/><text fill="#fafafa"/><b fill="#ef4444"/>"##
        );
        assert_eq!(Theme::default().recolor(&svg), svg.replace(ON_ACCENT, "#ffffff"));
        // A light accent gets dark text on it.
        assert!(
            Theme::from_settings("", "#facc15")
                .recolor(&svg)
                .contains(&format!(r#"<text fill="{DARK_TEXT}"/>"#))
        );
    }

    #[test]
    fn finish_adds_badge_and_theme() {
        let uri = render::message_image("Claude", "offline", render::DANGER);
        assert_eq!(finish(&uri, &Theme::default(), None), uri);
        let badge = Badge {
            color: render::WARN,
            text: "16m ago".into(),
        };
        let svg = render::svg_from_data_uri(&finish(&uri, &Theme::from_settings("#000000", ""), Some(&badge))).unwrap();
        assert!(svg.contains(r##"fill="#000000""##) && svg.contains("16m ago") && svg.contains(r#"cx="136""#));
    }
}
