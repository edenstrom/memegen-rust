//! `memegen bench`: wall-clock and CPU time per render.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::app::App;
use crate::render::MemeRequest;

pub struct Case {
    pub name: &'static str,
    pub template: &'static str,
    pub lines: &'static [&'static str],
    pub extension: &'static str,
}

pub const CASES: &[Case] = &[
    Case {
        name: "static png, 2 lines",
        template: "fry",
        lines: &["not sure if trolling", "or just stupid"],
        extension: "png",
    },
    Case {
        name: "static jpg, wrapped text",
        template: "ski",
        lines: &[
            "if you try to put a bunch more text than can possibly fit on a meme",
            "you're gonna have a bad time",
        ],
        extension: "jpg",
    },
    Case {
        name: "static png, 3 lines rotated",
        template: "ds",
        lines: &[
            "Push this button.",
            "Push that button.",
            "can't decide which is worse",
        ],
        extension: "png",
    },
    Case {
        name: "static png, emoji",
        template: "fry",
        lines: &[":fire: this is fine :thumbsup:", "emoji ok"],
        extension: "png",
    },
    Case {
        name: "animated gif (17 frames)",
        template: "oprah",
        lines: &["you get animated text", "and you get animated text"],
        extension: "gif",
    },
    Case {
        name: "animated webp (17 frames)",
        template: "oprah",
        lines: &["you get animated text", "and you get animated text"],
        extension: "webp",
    },
];

/// Process CPU time (user + system, all threads).
fn cpu_time() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn measure(mut f: impl FnMut() -> Result<()>) -> Result<(Duration, Duration)> {
    let (wall, cpu) = (Instant::now(), cpu_time());
    f()?;
    Ok((wall.elapsed(), cpu_time() - cpu))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

pub fn run(app: Arc<App>, iterations: usize) -> Result<()> {
    let renderer = &app.renderer;
    println!(
        "{:<30} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "case", "cold wall", "cold cpu", "warm wall", "warm cpu", "cached", "size"
    );
    for case in CASES {
        let template = renderer
            .catalog()
            .get(case.template)
            .context("missing template")?
            .clone();
        let lines: Vec<String> = case.lines.iter().map(|s| s.to_string()).collect();

        // Cold: decode + resize the background, then render and encode.
        let mut cold = (vec![], vec![]);
        for _ in 0..iterations.clamp(1, 5) {
            renderer.clear_caches();
            let (wall, cpu) = measure(|| {
                renderer
                    .render_bytes(&template, &lines, "", case.extension)
                    .map(drop)
            })?;
            cold.0.push(ms(wall));
            cold.1.push(ms(cpu));
        }

        // Warm: background cached, output not cached (a new meme every time).
        let mut size = 0;
        renderer.render_bytes(&template, &lines, "", case.extension)?;
        let mut warm = (vec![], vec![]);
        for _ in 0..iterations {
            let (wall, cpu) = measure(|| {
                size = renderer
                    .render_bytes(&template, &lines, "", case.extension)?
                    .len();
                Ok(())
            })?;
            warm.0.push(ms(wall));
            warm.1.push(ms(cpu));
        }

        // Cached: the same meme requested again (in-memory output cache).
        let request = MemeRequest {
            template_id: case.template.into(),
            lines: lines.clone(),
            font: String::new(),
            extension: case.extension.into(),
        };
        renderer.render(&request).ok();
        let mut cached = vec![];
        for _ in 0..iterations {
            let (wall, _) = measure(|| renderer.render(&request).map(drop).map_err(Into::into))?;
            cached.push(ms(wall));
        }

        println!(
            "{:<30} {:>8.2} ms {:>8.2} ms {:>8.2} ms {:>8.2} ms {:>8.4} ms {:>8} KB",
            case.name,
            median(cold.0),
            median(cold.1),
            median(warm.0),
            median(warm.1),
            median(cached),
            size / 1024
        );
    }
    Ok(())
}
