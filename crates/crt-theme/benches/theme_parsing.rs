//! Criterion benchmarks for theme CSS parsing.
//!
//! Run with: cargo bench -p crt-theme

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crt_theme::parser::{parse_theme, parse_theme_report};

/// The smallest useful theme: foreground and background only.
const MINIMAL_THEME: &str = r#"
:terminal {
    color: #e0e0e0;
    background: #1a1a2e;
}
"#;

/// Typography, gradient background, glow, cursor, selection and a full ANSI palette.
const MEDIUM_THEME: &str = r#"
:terminal {
    font-family: "JetBrains Mono", monospace;
    font-size: 14px;
    line-height: 1.4;
    color: #e0e0e0;
    background: linear-gradient(180deg, #0a0a1a, #1a1a2e);
    text-shadow: 0 0 8px rgba(0, 255, 65, 0.6);

    --ansi-black: #1a1a2e;
    --ansi-red: #ff0055;
    --ansi-green: #00ff41;
    --ansi-yellow: #f0e68c;
    --ansi-blue: #00bfff;
    --ansi-magenta: #ff00ff;
    --ansi-cyan: #00ffff;
    --ansi-white: #e0e0e0;
    --ansi-bright-black: #444466;
    --ansi-bright-red: #ff3377;
    --ansi-bright-green: #33ff66;
    --ansi-bright-yellow: #ffff88;
    --ansi-bright-blue: #33ccff;
    --ansi-bright-magenta: #ff33ff;
    --ansi-bright-cyan: #33ffff;
    --ansi-bright-white: #ffffff;
}

:terminal::cursor {
    background: #00ff41;
    text-shadow: 0 0 10px rgba(0, 255, 65, 0.8);
}

:terminal::selection {
    background: rgba(0, 255, 65, 0.3);
    color: #ffffff;
}

:terminal::highlight {
    background: rgba(255, 255, 0, 0.3);
    color: #ffffff;
    --current-background: rgba(255, 165, 0, 0.5);
}
"#;

/// Everything in the medium theme plus backdrop effects, an extended palette and event overrides.
const FULL_THEME: &str = r#"
:terminal {
    font-family: "JetBrains Mono", monospace;
    font-size: 14px;
    line-height: 1.4;
    color: #e0e0e0;
    background: linear-gradient(180deg, #0a0a1a, #1a1a2e);
    text-shadow: 0 0 8px rgba(0, 255, 65, 0.6);

    --ansi-black: #1a1a2e;
    --ansi-red: #ff0055;
    --ansi-green: #00ff41;
    --ansi-yellow: #f0e68c;
    --ansi-blue: #00bfff;
    --ansi-magenta: #ff00ff;
    --ansi-cyan: #00ffff;
    --ansi-white: #e0e0e0;
    --ansi-bright-black: #444466;
    --ansi-bright-red: #ff3377;
    --ansi-bright-green: #33ff66;
    --ansi-bright-yellow: #ffff88;
    --ansi-bright-blue: #33ccff;
    --ansi-bright-magenta: #ff33ff;
    --ansi-bright-cyan: #33ffff;
    --ansi-bright-white: #ffffff;
}

:terminal::cursor {
    background: #00ff41;
    text-shadow: 0 0 10px rgba(0, 255, 65, 0.8);
}

:terminal::selection {
    background: rgba(0, 255, 65, 0.3);
    color: #ffffff;
}

:terminal::backdrop {
    --grid-enabled: true;
    --grid-color: rgba(255, 0, 255, 0.2);
    --grid-spacing: 6;
    --grid-line-width: 2.5;
    --grid-perspective: 3.5;
    --grid-horizon: 0.8;
    --grid-animation-speed: 0.2;
    --grid-glow-radius: 7;
    --grid-glow-intensity: 0.8;

    --starfield-color: #ffffff;
    --starfield-density: 150;
    --starfield-layers: 3;
    --starfield-speed: 0.3;
    --starfield-twinkle: true;

    --crt-enabled: true;
    --crt-scanline-intensity: 0.15;
    --crt-curvature: 0.02;
    --crt-vignette: 0.3;
    --crt-flicker: 0.03;
}

:terminal::palette {
    --color-0: #1a0a20;
    --color-1: #ff0055;
    --color-2: #00ff88;
    --color-3: #ffcc00;
    --color-4: #00ccff;
    --color-5: #ff00ff;
    --color-6: #00ffff;
    --color-7: #e0e0e0;
    --color-8: #444466;
    --color-9: #ff3377;
    --color-10: #33ff66;
    --color-11: #ffff88;
    --color-12: #33ccff;
    --color-13: #ff33ff;
    --color-14: #33ffff;
    --color-15: #ffffff;
    --color-196: #ff0000;
    --color-226: #ffff00;
}

:terminal::on-bell {
    --duration: 300ms;
    --flash-color: rgba(255, 0, 0, 0.3);
    --flash-intensity: 0.5;
    --cursor-color: #ff0000;
}

:terminal::on-command-success {
    --duration: 200ms;
    --flash-color: rgba(0, 255, 0, 0.15);
    --flash-intensity: 0.3;
}

:terminal::on-command-fail {
    --duration: 500ms;
    --flash-color: rgba(255, 0, 0, 0.2);
    --flash-intensity: 0.4;
    --starfield-color: rgba(255, 100, 50, 0.9);
    --starfield-speed: 0.3;
}
"#;

/// The bundled synthwave theme, as shipped in `assets/themes`.
const SYNTHWAVE_THEME: &str = include_str!("../../../assets/themes/synthwave.css");

fn bench_parse_theme(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_theme");

    for (name, css) in [
        ("minimal", MINIMAL_THEME),
        ("medium", MEDIUM_THEME),
        ("full", FULL_THEME),
        ("synthwave", SYNTHWAVE_THEME),
    ] {
        // Fail loudly if a fixture stops parsing rather than benchmarking an error path.
        parse_theme(css).expect("benchmark theme must parse");

        group.bench_with_input(BenchmarkId::new("css", name), &css, |b, css| {
            b.iter(|| parse_theme(css).unwrap());
        });
    }

    group.bench_with_input(
        BenchmarkId::new("report", "synthwave"),
        &SYNTHWAVE_THEME,
        |b, css| {
            b.iter(|| parse_theme_report(css).unwrap());
        },
    );

    group.finish();
}

criterion_group!(benches, bench_parse_theme);
criterion_main!(benches);
