//! `runnerctl tui`: a full-screen dashboard of the daemon, for hosts reached over SSH. A pure API client like the
//! rest of `runnerctl`: a snapshot from `/v1/*`, then `/v1/events`. This module runs the loop: it feeds keys, events
//! and answers to `app`'s reducer, makes the requests the reducer asks for, and draws with `draw` after each batch
//! and every second, at most once per `FRAME`. It decides nothing itself. Whatever ends or stops it (`q`, SIGINT,
//! SIGTERM, SIGHUP, Ctrl-Z, SIGTSTP) gives the terminal back first; SIGCONT takes it again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::event::{self as terminal, Event as TerminalEvent};
use runnerd_core::api::{
    EventQuery, HostSampleQuery, HostSamples, Job, JobPage, PauseQuery, Paused, Records,
    SampleSeries, Sizing, Status,
};
use runnerd_core::client::{Client, ClientError, Events, StatusCode};
use runnerd_core::config::ByteSize;
use tokio::signal::unix::{Signal as Stream, SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{Error, now, output, watch};

mod app;
mod draw;
#[cfg(test)]
mod tests;

use app::{App, Effect, LOG_LINES, Msg, Refused, Removal, Resizing};
use draw::Theme;

/// The shortest time between two frames, about 30 a second.
const FRAME: Duration = Duration::from_millis(33);
/// How long the feed waits between attempts to reach a daemon that went away.
const RETRY: Duration = Duration::from_secs(2);
/// How far back the slice's charts start.
const HOST_HISTORY: u64 = 30 * 60;

/// Reads the snapshot, takes over the terminal and runs until `q`. The terminal is given back however it ends.
pub(crate) async fn run(client: &Client, colour: bool) -> Result<(), Error> {
    let (events, mut app) = snapshot(client).await?;
    // Before raw mode, so that no signal can end the process with the terminal raw.
    let mut signals = Signals::new().map_err(|error| local(&error))?;
    let (send, mut receive) = mpsc::unbounded_channel();
    let feed = tokio::spawn(feed(client.clone(), events, send.clone(), RETRY));
    let keys = Keys::spawn(send.clone());
    let mut terminal = match output::full_screen() {
        Ok(terminal) => terminal,
        Err(error) => {
            // Raw mode may be on even though the alternate screen failed.
            let _ = output::restore();
            keys.stop();
            feed.abort();
            return Err(local(&error));
        }
    };
    let mut requests = Requests {
        client: client.clone(),
        send,
        log: None,
    };
    let theme = Theme { colour };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // Drawn when something changed, at most once a frame: a burst of events is applied at once and drawn once.
    let mut changed = true;
    let mut next_frame = Instant::now();
    let ended = loop {
        if changed && Instant::now() >= next_frame {
            if let Err(error) = terminal.draw(|frame| draw::draw(frame, &app, now(), theme)) {
                break Err(local(&error));
            }
            changed = false;
            next_frame = Instant::now() + FRAME;
        }
        let mut msg = tokio::select! {
            msg = receive.recv() => match msg {
                Some(msg) => msg,
                None => break Ok(()),
            },
            received = signals.next() => {
                let taken = match received {
                    Received::Quit => break Ok(()),
                    Received::Suspend => suspend(&mut terminal),
                    Received::Continue => resume(&mut terminal),
                };
                if let Err(error) = taken {
                    break Err(local(&error));
                }
                changed = true;
                continue;
            },
            _ = tick.tick() => {
                changed = true;
                continue;
            },
            () = tokio::time::sleep_until(next_frame), if changed => continue,
        };
        changed = true;
        // Everything already waiting is applied before the next frame.
        let mut quit = false;
        let mut failed = None;
        loop {
            for effect in app.update(msg) {
                match effect {
                    Effect::Quit => quit = true,
                    Effect::Suspend => failed = suspend(&mut terminal).err().or(failed),
                    effect => requests.make(effect),
                }
            }
            if quit || failed.is_some() {
                break;
            }
            match receive.try_recv() {
                Ok(next) => msg = next,
                Err(_) => break,
            }
        }
        if let Some(error) = failed {
            break Err(local(&error));
        }
        if quit {
            break Ok(());
        }
    };
    let restored = output::restore().map_err(|error| local(&error));
    keys.stop();
    feed.abort();
    requests.close_log();
    ended.and(restored)
}

/// The event stream and the state it starts from. The stream opens first, so nothing that happens while the
/// snapshot is read is missed.
async fn snapshot(client: &Client) -> Result<(Events, App), Error> {
    let events = client.events(EventQuery::default(), None).await?;
    let live = watch::State::read(client).await?;
    let host: HostSamples = client
        .get_query(
            "/v1/host/samples",
            &HostSampleQuery {
                since: Some(now().saturating_sub(HOST_HISTORY)),
            },
        )
        .await?;
    let mut series = Vec::new();
    for id in live.running.keys() {
        series.push(client.get(&format!("/v1/jobs/{id}/samples")).await?);
    }
    Ok((events, App::new(live, host.samples, series)))
}

fn local(error: &std::io::Error) -> Error {
    Error::Local(format!("terminal: {error}"))
}

/// What a signal asks of the TUI.
enum Received {
    Quit,
    Suspend,
    Continue,
}

/// The signals the TUI answers. Registered, they no longer end or stop the process by themselves, so the terminal
/// is given back first.
struct Signals {
    interrupt: Stream,
    terminate: Stream,
    hangup: Stream,
    stop: Stream,
    resume: Stream,
}

impl Signals {
    fn new() -> std::io::Result<Signals> {
        Ok(Signals {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            stop: signal(SignalKind::from_raw(rustix::process::Signal::TSTP.as_raw()))?,
            resume: signal(SignalKind::from_raw(rustix::process::Signal::CONT.as_raw()))?,
        })
    }

    /// The next signal; cancel-safe.
    async fn next(&mut self) -> Received {
        tokio::select! {
            _ = self.interrupt.recv() => Received::Quit,
            _ = self.terminate.recv() => Received::Quit,
            _ = self.hangup.recv() => Received::Quit,
            _ = self.stop.recv() => Received::Suspend,
            _ = self.resume.recv() => Received::Continue,
        }
    }
}

/// Gives the terminal back and stops, as Ctrl-Z does in a shell; once continued, takes the terminal again.
fn suspend(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    output::restore()?;
    // The process group, as the terminal's own Ctrl-Z would; SIGSTOP, because SIGTSTP now comes back here.
    rustix::process::kill_current_process_group(rustix::process::Signal::STOP)?;
    resume(terminal)
}

/// Takes the terminal again and draws all of it at the next frame, since the shell or another program drew over it.
fn resume(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    *terminal = output::resume()?;
    Ok(())
}

/// Reads events into the loop. When the stream fails for good (the daemon stopped), it says so and tries again
/// every `retry`. With an event read before, the new stream resumes after it and replays what was missed. Without
/// one there is nothing to replay from, so it reads the state again once the new stream is open, whether this feed
/// opened it or `Events` did, by itself, after a short break. It owns the stream, so a read is never dropped half-way
/// and the stream's idle timeout is not restarted by redraws.
async fn feed(client: Client, mut events: Events, send: UnboundedSender<Msg>, retry: Duration) {
    let mut last = None;
    loop {
        let read = events.next().await;
        // Read after the new stream opened, so it misses nothing; the event read from it comes after.
        let reread = if events.missed_events() {
            watch::State::read(&client).await.map(Some)
        } else {
            Ok(None)
        };
        let failed = match (read, reread) {
            (Ok(envelope), Ok(state)) => {
                if let Some(state) = state
                    && send.send(Msg::Snapshot(Box::new(state))).is_err()
                {
                    return;
                }
                if envelope.id.is_some() {
                    last = envelope.id;
                }
                if send.send(Msg::Event(envelope)).is_err() {
                    return;
                }
                continue;
            }
            (Err(error), _) | (_, Err(error)) => error,
        };
        if send.send(Msg::Disconnected(short(&failed))).is_err() {
            return;
        }
        match reopen(&client, last, &send, retry).await {
            Some(reopened) => events = reopened,
            None => return,
        }
        if send.send(Msg::Reconnected).is_err() {
            return;
        }
    }
}

/// Opens the stream again, every `retry` until it opens: after `last`, or else with the state read again. `None`
/// once the loop is gone.
async fn reopen(
    client: &Client,
    last: Option<u64>,
    send: &UnboundedSender<Msg>,
    retry: Duration,
) -> Option<Events> {
    loop {
        tokio::time::sleep(retry).await;
        let reopened = match client.events(EventQuery::default(), last).await {
            Ok(events) if last.is_some() => Ok((events, None)),
            // Opened before the state is read, so nothing that happens meanwhile is missed.
            Ok(events) => watch::State::read(client)
                .await
                .map(|state| (events, Some(state))),
            Err(error) => Err(error),
        };
        match reopened {
            Ok((events, state)) => {
                if let Some(state) = state {
                    send.send(Msg::Snapshot(Box::new(state))).ok()?;
                }
                return Some(events);
            }
            Err(error) => send.send(Msg::Disconnected(short(&error))).ok()?,
        }
    }
}

/// An error short enough for the header.
fn short(error: &ClientError) -> String {
    match error {
        ClientError::NotRunning(_) => "runnerd is not running".to_owned(),
        error => error.to_string(),
    }
}

/// Reads keys on a thread of its own, since the terminal's reads block.
struct Keys {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Keys {
    fn spawn(send: UnboundedSender<Msg>) -> Keys {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                // Polled, not read, so that `stop` is seen within a tenth of a second.
                match terminal::poll(Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(_) => return,
                }
                let msg = match terminal::read() {
                    Ok(TerminalEvent::Key(key)) => Msg::Key(key),
                    Ok(TerminalEvent::Resize(..)) => Msg::Resize,
                    Ok(_) => continue,
                    Err(_) => return,
                };
                if send.send(msg).is_err() {
                    return;
                }
            }
        });
        Keys { stop, thread }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Makes the requests the reducer asks for, each in a task of its own whose answer comes back as a message, so the
/// screen keeps drawing while the daemon works.
struct Requests {
    client: Client,
    send: UnboundedSender<Msg>,
    /// The open job's log reader.
    log: Option<JoinHandle<()>>,
}

impl Requests {
    /// Starts the request.
    fn make(&mut self, effect: Effect) {
        let client = self.client.clone();
        let send = self.send.clone();
        match effect {
            // The loop's own, since they act on the terminal.
            Effect::Quit | Effect::Suspend => {}
            Effect::OpenJob(id) => {
                self.close_log();
                self.log = Some(tokio::spawn(open_job(client, send, id)));
            }
            Effect::CloseJob => self.close_log(),
            Effect::RefreshJob(id) => {
                tokio::spawn(async move {
                    answer(
                        &send,
                        client.get::<Job>(&format!("/v1/jobs/{id}")).await,
                        Msg::Job,
                    );
                });
            }
            Effect::History { generation, query } => {
                tokio::spawn(async move {
                    let page = client
                        .get_query::<JobPage>("/v1/jobs", &query)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = send.send(Msg::History { generation, page });
                });
            }
            Effect::Pause { repository, force } => {
                tokio::spawn(async move {
                    let msg = pause(&client, repository, force).await;
                    let _ = send.send(msg);
                });
            }
            Effect::Resume { repository } => {
                tokio::spawn(async move {
                    let resumed = match crate::Target::new(repository) {
                        Ok(target) => client
                            .post(&format!("{}/resume", target.path), &())
                            .await
                            .map(|_| target.name)
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = send.send(match resumed {
                        Ok(name) => Msg::Done(format!("{name} resumed")),
                        Err(error) => Msg::Failed(error),
                    });
                });
            }
            Effect::Records => {
                tokio::spawn(async move {
                    answer(
                        &send,
                        client.get::<Records>("/v1/records").await,
                        Msg::Records,
                    );
                });
            }
            Effect::RemoveRecords(removal) => {
                tokio::spawn(async move {
                    let path = match removal {
                        Removal::Job(job) => format!("/v1/records/{job}"),
                        Removal::All => "/v1/records?all=true".to_owned(),
                    };
                    let removed = crate::remove_records(&client, &path).await;
                    let _ = send.send(match removed {
                        Ok(removed) => Msg::Done(crate::human::removed(&removed)),
                        Err(error) => Msg::Failed(error.to_string()),
                    });
                    answer(
                        &send,
                        client.get::<Records>("/v1/records").await,
                        Msg::Records,
                    );
                });
            }
            Effect::Sizing => {
                tokio::spawn(async move {
                    answer(&send, client.get::<Sizing>("/v1/sizing").await, Msg::Sizing);
                });
            }
            Effect::Resize(resizing) => {
                tokio::spawn(write_resize(client, send, resizing));
            }
            Effect::Reseed { profile } => {
                tokio::spawn(async move {
                    let requested = client
                        .post(&format!("/v1/profiles/{profile}/reseed"), &())
                        .await;
                    status(&client, &send).await;
                    let _ = send.send(match requested {
                        Ok(_) => Msg::Done(format!(
                            "{profile}: reseed requested; it runs once the profile's slots are free"
                        )),
                        Err(error) => Msg::Failed(error.to_string()),
                    });
                });
            }
        }
    }

    fn close_log(&mut self) {
        if let Some(log) = self.log.take() {
            log.abort();
        }
    }
}

/// Sends what a request answered, or why it failed.
fn answer<T>(send: &UnboundedSender<Msg>, answer: Result<T, ClientError>, msg: fn(T) -> Msg) {
    let _ = send.send(match answer {
        Ok(value) => msg(value),
        Err(error) => Msg::Failed(error.to_string()),
    });
}

/// A profile's reseeding flag comes with the status only, so it is read again after a reseed request.
async fn status(client: &Client, send: &UnboundedSender<Msg>) {
    answer(send, client.get::<Status>("/v1/status").await, Msg::Status);
}

/// Writes a profile's advised memory and slots into the daemon's config, says how that went, and reads the advice
/// again.
async fn write_resize(client: Client, send: UnboundedSender<Msg>, resizing: Resizing) {
    let Resizing { profile, resize } = resizing;
    let _ = send.send(match crate::write_resize(&client, &profile, resize).await {
        Ok(_) => Msg::Done(format!(
            "{profile}: memory {}, {} slots written; the daemon runs with them",
            ByteSize(resize.memory),
            resize.slots
        )),
        Err(error) => Msg::Failed(error.to_string()),
    });
    answer(&send, client.get::<Sizing>("/v1/sizing").await, Msg::Sizing);
}

/// Pauses `repository`, or every one when `None`.
async fn pause(client: &Client, repository: Option<String>, force: bool) -> Msg {
    let target = match crate::Target::new(repository.clone()) {
        Ok(target) => target,
        Err(error) => return Msg::Failed(error.to_string()),
    };
    let name = &target.name;
    match client
        .post_json::<Paused>(&format!("{}/pause", target.path), &PauseQuery { force })
        .await
    {
        Ok(paused) if paused.stopped.is_empty() => {
            Msg::Done(format!("{name} paused: it gets no new runners"))
        }
        Ok(paused) => Msg::Done(format!(
            "{name} paused; stopped {}",
            paused
                .stopped
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(ClientError::Api {
            status,
            message,
            jobs,
        }) if status == StatusCode::CONFLICT => Msg::Busy(Refused {
            repository,
            message,
            jobs: jobs.len(),
        }),
        Err(error) => Msg::Failed(error.to_string()),
    }
}

/// Reads the job, its samples and its log; follows the log until the job's record is final.
async fn open_job(client: Client, send: UnboundedSender<Msg>, id: u64) {
    let job = match client.get::<Job>(&format!("/v1/jobs/{id}")).await {
        Ok(job) => job,
        Err(error) => {
            let _ = send.send(Msg::Failed(error.to_string()));
            return;
        }
    };
    let follow = !job.summary.state.is_final();
    let from = job.summary.log.lines.saturating_sub(LOG_LINES as u64);
    let _ = send.send(Msg::Job(job));
    answer(
        &send,
        client
            .get::<SampleSeries>(&format!("/v1/jobs/{id}/samples"))
            .await,
        Msg::Samples,
    );
    let lines = send.clone();
    let read = crate::logs(&client, id, follow, from, move |text| {
        lines.send(Msg::LogLine { job: id, text }).is_ok()
    })
    .await;
    if let Err(error) = read {
        let _ = send.send(Msg::Failed(format!("log of job {id}: {error}")));
    }
}
