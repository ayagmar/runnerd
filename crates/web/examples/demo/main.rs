//! The dashboard against made-up data, for working on its design: `cargo run -p runnerd-web --example demo -- DIR
//! [ADDR [ORIGIN...]]` serves a fake daemon's socket in DIR and the dashboard on ADDR (default 127.0.0.1:7478),
//! taking writes from each ORIGIN as from `[web] origins` too, and sends a sample of each running job and of the
//! slice every 2 seconds, and a log line every second. Open the first link in `DIR/runnerd.web`.

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::expect_used,
    reason = "a development tool: it prints where to look and stops on any failure"
)]

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runnerd_core::api::{Event, EventEnvelope, HostSample, JobState, Sample};

mod fake;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: demo DIR [ADDR]"));
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7478".to_owned());
    let proxies = args.map(|origin| origin.parse().unwrap()).collect();
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("runnerd.sock");
    let _ = std::fs::remove_file(&socket);
    let fake = fake::Fake::new(fake::Fixtures::new(now()));
    fake::serve(&socket, fake.clone());
    let listener = runnerd_web::listen(&addr).await.unwrap().unwrap();
    let sessions = runnerd_web::Sessions::open(dir.join("dashboard-session")).unwrap();
    let web = runnerd_web::spawn(
        listener,
        socket.clone(),
        proxies,
        sessions,
        std::future::pending::<()>(),
    );
    println!(
        "login links in {}",
        runnerd_web::login_file(&socket).display()
    );
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut n = 0u64;
    loop {
        tick.tick().await;
        n += 1;
        let at = now();
        let mut events = Vec::new();
        {
            let mut fixtures = fake.fixtures.lock().unwrap();
            let running: Vec<u64> = fixtures
                .jobs
                .iter()
                .filter(|job| job.summary.state == JobState::Running)
                .map(|job| job.summary.id)
                .collect();
            for id in running {
                let line = fixtures.logs.get(&id).map_or(0, Vec::len) as u64;
                let text = format!("[{n:>4}] progress of the running step");
                fixtures.logs.entry(id).or_default().push(text.clone());
                if let Some(job) = fixtures.jobs.iter_mut().find(|job| job.summary.id == id) {
                    job.summary.log.lines += 1;
                    job.summary.log.bytes += text.len() as u64 + 1;
                }
                events.push(Event::LogLine {
                    job: id,
                    line,
                    text,
                });
                if n.is_multiple_of(2) {
                    let series = fixtures.samples.get(&id);
                    let first = series.and_then(|s| s.first()).map(|s| s.memory);
                    let last = series.and_then(|s| s.last()).cloned();
                    if let (Some(first), Some(last)) = (first, last) {
                        let base = first * 3 / 2;
                        let sample = Sample {
                            at,
                            cpu: 1500 + n * 6151 % 5200,
                            memory: base + (n * 7919 % 97) * (base / 600),
                            network_received: last.network_received + 400_000,
                            network_sent: last.network_sent + 40_000,
                        };
                        fixtures.samples.entry(id).or_default().push(sample.clone());
                        events.push(Event::JobSample { job: id, sample });
                    }
                }
            }
            if n.is_multiple_of(2) {
                let base = 9 << 30;
                let sample = HostSample {
                    at,
                    cpu: 2500 + n * 4271 % 4800,
                    memory: base + (n * 104_729 % 89) * (base / 300),
                };
                fixtures.host.push(sample.clone());
                events.push(Event::HostSample { sample });
            }
        }
        for event in events {
            let _ = fake.events.send(EventEnvelope {
                id: None,
                at,
                event,
            });
        }
        if web.is_finished() {
            return;
        }
    }
}
