//! Offline Pele rendering. Called only by background workers after an explicit
//! preview action; neither JavaScript nor SVG/font work runs in a frame.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use gpui::{Hsla, RenderImage, Rgba, SvgRenderer};
use rquickjs::{Context, Exception, Function, Runtime};
use serde::{Deserialize, Serialize};

use crate::theme::Theme;

mod preview;
pub(super) use preview::open;

const PELE: &str = concat!(
    "/*\n",
    include_str!("../../assets/mermaid/LICENSE"),
    "\n*/\n",
    include_str!("../../assets/mermaid/pele.global.js"),
);
const BRIDGE: &str = include_str!("mermaid.js");
const MAX_SOURCE_BYTES: usize = 128 * 1024;
const MAX_OUTPUT_BYTES: usize = 2_000_000;
const TIME_LIMIT: Duration = Duration::from_secs(2);
const MAX_RASTER_PIXELS: f64 = 8_000_000.0;

pub(super) fn is_mermaid(language: Option<&str>) -> bool {
    language.is_some_and(|language| language.eq_ignore_ascii_case("mermaid"))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Colors {
    background: String,
    text: String,
    muted: String,
    surface: String,
    surface_alt: String,
    border: String,
    accent: String,
    danger: String,
}

impl Colors {
    fn from_theme(theme: Theme) -> Self {
        let background: Rgba = theme.canvas.into();
        let css = |color: Hsla| {
            let color: Rgba = color.into();
            let channel = |ink: f32, paper: f32| {
                ((ink * color.a + paper * (1.0 - color.a)).clamp(0.0, 1.0) * 255.0).round() as u8
            };
            format!(
                "#{:02x}{:02x}{:02x}",
                channel(color.r, background.r),
                channel(color.g, background.g),
                channel(color.b, background.b)
            )
        };
        Self {
            background: css(theme.canvas),
            text: css(theme.text),
            muted: css(theme.text_secondary),
            surface: css(theme.raised),
            surface_alt: css(theme.inset),
            border: css(theme.border_strong),
            accent: css(theme.accent),
            danger: css(theme.danger),
        }
    }
}

#[derive(Deserialize)]
struct Diagram {
    svg: String,
    width: f64,
    height: f64,
}

struct Rendered {
    svg: Arc<str>,
    image: Arc<RenderImage>,
    width: f32,
    height: f32,
}

fn javascript_error(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Ok(exception) = Exception::from_value(caught)
            && let Some(message) = exception.message()
        {
            return message.chars().take(2000).collect();
        }
    }
    error.to_string()
}

fn diagram(source: &str, colors: &Colors, cancel: Arc<AtomicBool>) -> Result<Diagram, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(tr!("mermaid.source_too_large"));
    }
    let deadline = Instant::now() + TIME_LIMIT;
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime.set_memory_limit(128 * 1024 * 1024);
    runtime.set_max_stack_size(2 * 1024 * 1024);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        cancel.load(Ordering::Relaxed) || Instant::now() >= deadline
    })));
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;
    let json = context
        .with(|ctx| {
            ctx.eval::<(), _>(PELE)
                .map_err(|error| javascript_error(&ctx, error))?;
            ctx.eval::<(), _>(BRIDGE)
                .map_err(|error| javascript_error(&ctx, error))?;
            let colors = serde_json::to_vec(colors).map_err(|error| error.to_string())?;
            let colors = ctx
                .json_parse(colors)
                .map_err(|error| javascript_error(&ctx, error))?;
            let render: Function = ctx
                .globals()
                .get("wakuRenderMermaid")
                .map_err(|error| javascript_error(&ctx, error))?;
            render
                .call::<_, String>((source, colors))
                .map_err(|error| javascript_error(&ctx, error))
        })
        .map_err(|error| {
            if Instant::now() >= deadline {
                tr!("mermaid.timed_out")
            } else {
                error
            }
        })?;
    let result: Diagram = serde_json::from_str(&json).map_err(|error| error.to_string())?;
    if result.svg.len() > MAX_OUTPUT_BYTES {
        return Err(tr!("mermaid.diagram_too_large"));
    }
    raster_scale(result.width, result.height, 1.0)?;
    Ok(result)
}

fn raster_scale(width: f64, height: f64, display_scale: f32) -> Result<f32, String> {
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > 16384.0
        || height > 16384.0
    {
        return Err(tr!("mermaid.diagram_too_large"));
    }
    // GPUI supersamples SVGs at 2x. Bound the actual pixel allocation,
    // including display scale, before invoking the SVG rasterizer.
    let physical_scale = (display_scale.clamp(1.0, 3.0) as f64 * 2.0)
        .min(4096.0 / width)
        .min(4096.0 / height)
        .min((MAX_RASTER_PIXELS / (width * height)).sqrt());
    Ok((physical_scale / 2.0) as f32)
}

fn render(
    source: &str,
    colors: &Colors,
    scale: f32,
    cancel: Arc<AtomicBool>,
    renderer: &SvgRenderer,
) -> Result<Rendered, String> {
    let diagram = diagram(source, colors, cancel.clone())?;
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let image = renderer
        .render_single_frame(
            diagram.svg.as_bytes(),
            raster_scale(diagram.width, diagram.height, scale)?,
        )
        .map_err(|error| error.to_string())?;
    Ok(Rendered {
        svg: diagram.svg.into(),
        image,
        width: diagram.width as f32,
        height: diagram.height as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str, theme: Theme) -> Result<Diagram, String> {
        diagram(
            source,
            &Colors::from_theme(theme),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn pele_renders_flowcharts_sequence_and_other_mermaid_diagram_types_offline() {
        for source in [
            "flowchart LR\n A[开始] --> B[完成]",
            "sequenceDiagram\n Alice->>Bob: Hello\n Bob-->>Alice: Hi",
            "classDiagram\n Animal <|-- Duck",
            "stateDiagram-v2\n [*] --> Active\n Active --> [*]",
            "pie title Pets\n \"Dogs\" : 3\n \"Cats\" : 2",
            "gantt\n dateFormat YYYY-MM-DD\n section Work\n Ship :2026-10-01, 3d",
            "gitGraph\n commit\n branch feature\n checkout feature\n commit",
            "timeline\n title History\n 2025 : Start\n 2026 : Ship",
        ] {
            let result =
                parse(source, Theme::light()).unwrap_or_else(|error| panic!("{source}: {error}"));
            assert!(result.svg.starts_with("<svg"));
            assert!(result.width > 0.0 && result.height > 0.0);
            assert!(!result.svg.contains("var("));
        }
    }

    #[test]
    fn diagram_colors_follow_the_theme_and_text_is_preserved() {
        let source = "flowchart LR\n A[\"var(--_fg) 中文 <script>alert(1)</script>\"] --> B[Done]";
        let light = parse(source, Theme::light()).unwrap();
        let dark = parse(source, Theme::dark()).unwrap();
        assert_ne!(light.svg, dark.svg);
        assert!(light.svg.contains("var(--_fg) 中文"));
        assert!(!light.svg.contains("<script>"));
        assert!(dark.svg.contains(&Colors::from_theme(Theme::dark()).text));
    }

    #[test]
    fn invalid_oversized_and_cancelled_diagrams_return_errors() {
        assert!(parse("not a Mermaid diagram", Theme::light()).is_err());
        assert!(parse("flowchart LR\n A[unfinished", Theme::light()).is_err());
        assert!(parse(&"x".repeat(MAX_SOURCE_BYTES + 1), Theme::light()).is_err());
        assert!(
            diagram(
                "flowchart LR\n A-->B",
                &Colors::from_theme(Theme::light()),
                Arc::new(AtomicBool::new(true))
            )
            .is_err()
        );
    }

    #[test]
    fn untrusted_links_and_external_images_are_disabled() {
        let result = parse(
            "flowchart LR\n A[Click] --> B[Done]\n click A href \"https://example.com\"",
            Theme::light(),
        )
        .unwrap();
        assert!(!result.svg.contains("href="));
        assert!(!result.svg.contains("<image"));
    }

    #[test]
    fn native_svg_rasterizer_draws_themed_diagrams_and_text() {
        let renderer = SvgRenderer::new(Arc::new(()));
        for theme in [Theme::light(), Theme::dark()] {
            let colors = Colors::from_theme(theme);
            let rendered = render(
                "flowchart LR\n A[Mermaid preview]",
                &colors,
                2.0,
                Arc::new(AtomicBool::new(false)),
                &renderer,
            )
            .unwrap();
            let bytes = rendered.image.as_bytes(0).expect("raster pixels");
            assert!(bytes.len() <= MAX_RASTER_PIXELS as usize * 4);
            let foreground = colors.text.trim_start_matches('#');
            let rgb = u32::from_str_radix(foreground, 16).unwrap();
            let expected = [
                (rgb & 255) as u8,
                ((rgb >> 8) & 255) as u8,
                ((rgb >> 16) & 255) as u8,
                255,
            ];
            assert!(
                bytes.chunks_exact(4).any(|pixel| pixel == expected),
                "labels must be rasterized in the theme foreground"
            );
        }
    }

    #[test]
    fn raster_memory_and_dimensions_are_bounded_at_every_display_scale() {
        for (width, height) in [(200.0, 64.0), (3000.0, 2000.0), (16000.0, 100.0)] {
            for display in [1.0, 2.0, 3.0] {
                let scale = raster_scale(width, height, display).unwrap() as f64 * 2.0;
                assert!(width * scale <= 4096.01 && height * scale <= 4096.01);
                assert!(width * height * scale * scale <= MAX_RASTER_PIXELS + 1.0);
            }
        }
        for width in [0.0, -1.0, f64::NAN, f64::INFINITY, 20000.0] {
            assert!(raster_scale(width, 100.0, 2.0).is_err());
        }
        assert!(is_mermaid(Some("Mermaid")));
        assert!(!is_mermaid(Some("rust")));
    }
}
