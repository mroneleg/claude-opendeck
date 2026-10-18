//! Regenerates the PNG icons referenced by assets/manifest.json from the same
//! SVG renderer the plugin uses at runtime, rasterised with resvg (the renderer
//! OpenDeck itself uses). Run: `cargo run --features icons --bin build-icons`.

use std::path::Path;

use claude_opendeck::activity::Totals;
use claude_opendeck::render;
use claude_opendeck::usage::{BreakdownRow, Spend, Usage, Window};
use resvg::{tiny_skia, usvg};

fn main() -> anyhow::Result<()> {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
    std::fs::create_dir_all(&out)?;

    let now = 1_800_000_000_000;
    let hour = 3_600_000;
    let usage = Usage {
        session: Some(Window {
            label: "5h".into(),
            percent: 42.0,
            resets_at: Some(now + 2 * hour),
            window_ms: 5 * hour,
        }),
        weekly: Some(Window {
            label: "7d".into(),
            percent: 18.0,
            resets_at: Some(now + 72 * hour),
            window_ms: 168 * hour,
        }),
        scoped: vec![],
        breakdown: vec![],
        spend: None,
        extra_usage: None,
        fetched_at: now,
    };
    let rows = [("Claude Code", 70.0), ("Chats", 20.0), ("Cowork", 10.0), ("Other", 0.0)].map(|(name, percent)| {
        BreakdownRow {
            name: name.into(),
            percent,
        }
    });
    let spend = Spend {
        enabled: true,
        used: 12.5,
        limit: Some(50.0),
        currency: "USD".into(),
        percent: Some(25.0),
        disabled_reason: None,
    };
    let activity = Totals {
        input: 400,
        output: 180_000,
        cache_read: 11_600_000,
        cache_create: 450_000,
        messages: 0,
    };

    let from_uri = |uri: String| render::svg_from_data_uri(&uri).expect("our own data URI");
    let icons = [
        ("usage", from_uri(render::usage_image(&usage, "both", now))),
        ("breakdown", from_uri(render::breakdown_image(&rows))),
        ("extrausage", from_uri(render::spend_image(Some(&spend)))),
        ("needsyou", from_uri(render::needs_you_image(1, Some("my-project"), 0))),
        ("sessions", from_uri(render::sessions_image(3, 1))),
        ("activity", from_uri(render::activity_image(&activity))),
        ("launch", from_uri(render::launch_image("Claude"))),
        ("plugin", render::plugin_icon_svg()),
        ("category", render::category_icon_svg()),
    ];

    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    for (name, svg) in icons {
        let tree = usvg::Tree::from_str(&svg, &options)?;
        for (suffix, size) in [("", 72u32), ("@2x", 144)] {
            let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("non-zero size");
            let scale = size as f32 / tree.size().width();
            resvg::render(
                &tree,
                tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            let path = out.join(format!("{name}{suffix}.png"));
            pixmap.save_png(&path)?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}
