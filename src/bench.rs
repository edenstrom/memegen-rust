//! `memegen bench`: wall-clock and CPU time per render, or a load test of a
//! running server over HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Request, Uri, header};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use memegen::app::App;
use memegen::assets::Source;
use memegen::render::{AnimateText, MemeRequest};
use memegen::slug;

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
    // `fry` rather than `oprah`: memegen-rs maps `oprah` to a still image.
    Case {
        name: "animated gif (24 frames)",
        template: "fry",
        lines: &["not sure if animated", "or just slow"],
        extension: "gif",
    },
    Case {
        name: "animated webp (24 frames)",
        template: "fry",
        lines: &["not sure if animated", "or just slow"],
        extension: "webp",
    },
    Case {
        name: "animated mp4 (24 frames)",
        template: "fry",
        lines: &["not sure if animated", "or just slow"],
        extension: "mp4",
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

fn median(values: Vec<f64>) -> f64 {
    percentile(values, 0.5)
}

fn percentile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() as f64 * p) as usize).min(values.len() - 1)]
}

pub fn run(app: Arc<App>, source: &dyn Source, iterations: usize) -> Result<()> {
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
                    .render_bytes(
                        &template,
                        &lines,
                        "",
                        case.extension,
                        AnimateText::Off,
                        source,
                    )
                    .map(drop)
            })?;
            cold.0.push(ms(wall));
            cold.1.push(ms(cpu));
        }

        // Warm: background cached, output not cached (a new meme every time).
        let mut size = 0;
        renderer.render_bytes(
            &template,
            &lines,
            "",
            case.extension,
            AnimateText::Off,
            source,
        )?;
        let mut warm = (vec![], vec![]);
        for _ in 0..iterations {
            let (wall, cpu) = measure(|| {
                size = renderer
                    .render_bytes(
                        &template,
                        &lines,
                        "",
                        case.extension,
                        AnimateText::Off,
                        source,
                    )?
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
            animate_text: AnimateText::Off,
        };
        renderer.render(&request, source).ok();
        let mut cached = vec![];
        for _ in 0..iterations {
            let (wall, _) = measure(|| {
                renderer
                    .render(&request, source)
                    .map(drop)
                    .map_err(Into::into)
            })?;
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

/// Load-test a running memegen-compatible server (this one, upstream memegen,
/// or memegen-rs). Each connection sends requests back to back; a counter is
/// appended to the last line so every request is a new meme and no server
/// cache is hit.
pub async fn run_http(base: &str, connections: &[usize], duration: Duration) -> Result<()> {
    let uri: Uri = base.parse().context("invalid --url")?;
    let target = Target {
        authority: uri.authority().context("--url needs a host")?.to_string(),
        prefix: uri.path().trim_end_matches('/').to_string(),
        counter: AtomicUsize::new(0),
    };
    let target = Arc::new(target);
    println!(
        "{:<30} {:>5} {:>9} {:>11} {:>11} {:>11}",
        "case", "conns", "req/s", "p50", "p99", "size"
    );
    for case in CASES {
        // One request first so the server has the background decoded.
        if let Err(error) = connection(target.clone(), case, Instant::now()).await {
            println!("{:<30} {error:#}", case.name);
            continue;
        }
        for &conns in connections {
            let started = Instant::now();
            let deadline = started + duration;
            let tasks: Vec<_> = (0..conns)
                .map(|_| tokio::spawn(connection(target.clone(), case, deadline)))
                .collect();
            let (mut latencies, mut size) = (vec![], 0);
            for task in tasks {
                let (task_latencies, task_size) = task.await??;
                latencies.extend(task_latencies);
                size = task_size;
            }
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "{:<30} {:>5} {:>9.1} {:>8.2} ms {:>8.2} ms {:>8} KB",
                case.name,
                conns,
                latencies.len() as f64 / elapsed,
                percentile(latencies.clone(), 0.5),
                percentile(latencies, 0.99),
                size / 1024
            );
        }
    }
    Ok(())
}

struct Target {
    authority: String,
    prefix: String,
    counter: AtomicUsize,
}

/// Send requests over one keep-alive connection until `deadline` (at least
/// one), returning each latency in ms and the last response size.
async fn connection(
    target: Arc<Target>,
    case: &'static Case,
    deadline: Instant,
) -> Result<(Vec<f64>, usize)> {
    let stream = TcpStream::connect(&target.authority).await?;
    stream.set_nodelay(true)?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let mut latencies = vec![];
    loop {
        let n = target.counter.fetch_add(1, Ordering::Relaxed);
        let mut lines: Vec<String> = case.lines.iter().map(|s| s.to_string()).collect();
        if let Some(last) = lines.last_mut() {
            last.push_str(&format!(" {n}"));
        }
        let path = format!(
            "{}/images/{}/{}.{}",
            target.prefix,
            case.template,
            slug::encode(&lines),
            case.extension
        );
        let request = Request::get(&path)
            .header(header::HOST, &target.authority)
            .body(Empty::<Bytes>::new())?;
        let start = Instant::now();
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            bail!("GET {path}: {status}");
        }
        latencies.push(ms(start.elapsed()));
        if Instant::now() >= deadline {
            return Ok((latencies, body.len()));
        }
    }
}
