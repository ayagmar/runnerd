//! `runnerctl tui`: a full-screen dashboard of the daemon, for hosts reached over SSH. A pure API client like the
//! rest of `runnerctl`: a snapshot from `/v1/*`, then `/v1/events`. This module runs the loop: it feeds keys, events
//! and answers to `app`'s reducer, makes the requests the reducer asks for, and draws with `draw` after each batch
//! and every second, at most once per `FRAME`. It decides nothing itself. Whatever ends or stops it (`q`, SIGINT,
//! SIGTERM, SIGHUP, Ctrl-Z, SIGTSTP) gives the terminal back first; SIGCONT takes it again. A terminal that goes
//! away, as when an SSH connection drops, ends it as `q` does.

use std::convert::Infallible;
use std::io::IsTerminal;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{
    self as terminal, Event as TerminalEvent, MouseButton, MouseEvent, MouseEventKind,
};
use runnerd_core::api::{
    self, Activity, ActivityQuery, Caches, CompareQuery, ConfigCheck, ConfigFile, Doctor,
    EventQuery, HostSampleQuery, HostSamples, Insights, InsightsQuery, Job, JobList, JobQuery,
    LineRange, LogLinesQuery, LogQuery, PauseQuery, Paused, Records, SampleSeries, Savings, Sizing,
    Status, StyledLine,
};
use runnerd_core::client::{Client, ClientError, Events, JobActionAnswer, StatusCode};
use tokio::signal::unix::{Signal as Stream, SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::output::Mouse;
use crate::{Error, now, output, watch};

mod app;
mod draw;
mod log;
mod mouse;
mod overlay;
mod prompts;
mod reading;
#[cfg(test)]
mod tests;

use app::{App, ConfigRead, Effect, LOG_LINES, Msg, Refused, Removal, Resizing};
use draw::Theme;
use mouse::Hits;
use reading::Unread;

/// The shortest time between two frames, about 30 a second.
const FRAME: Duration = Duration::from_millis(33);
/// How long the feed waits between attempts to reach a daemon that went away.
const RETRY: Duration = Duration::from_secs(2);
/// How far back the slice's charts start.
const HOST_HISTORY: u64 = 30 * 60;
/// The window of the health figures: the last day.
const DAY: u64 = 24 * 3600;

/// Reads the snapshot, takes over the terminal and runs until `q`. The terminal is given back however it ends.
pub(crate) async fn run(client: &Client, colour: bool, mouse: Mouse) -> Result<(), Error> {
    let (events, mut app) = snapshot(client).await?;
    // Before raw mode, so that no signal can end the process with the terminal raw.
    let mut signals = Signals::new().map_err(|error| local(&error))?;
    let (send, mut receive) = mpsc::unbounded_channel();
    let feed = tokio::spawn(feed(client.clone(), events, send.clone(), RETRY));
    // Where the last frame drew what a click can reach, for the keys thread to find what a click lands on.
    let hits = Arc::new(Mutex::new(Hits::default()));
    let mut keys = Keys::spawn(send.clone(), hits.clone());
    let mut terminal = match output::full_screen(mouse) {
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
            let mut drawn = Hits::default();
            if let Err(error) = terminal.draw(|frame| drawn = draw::draw(frame, &app, now(), theme))
            {
                break Err(error);
            }
            if let Ok(mut hits) = hits.lock() {
                *hits = drawn;
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
                    Received::Suspend => suspend(&mut terminal, mouse),
                    Received::Continue => resume(&mut terminal, mouse),
                };
                if let Err(error) = taken {
                    break Err(error);
                }
                changed = true;
                continue;
            },
            () = keys.ended() => break Ok(()),
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
                    Effect::Suspend => failed = suspend(&mut terminal, mouse).err().or(failed),
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
            break Err(error);
        }
        if quit {
            break Ok(());
        }
    };
    let restored = give_back(terminal);
    keys.stop();
    feed.abort();
    requests.close_log();
    unless_gone(ended)
        .and(unless_gone(restored))
        .map_err(|error| local(&error))
}

/// Gives the terminal back for good.
fn give_back(terminal: ratatui::DefaultTerminal) -> std::io::Result<()> {
    let restored = output::restore();
    // Its drop shows the cursor, which `restore` did, and prints a failure on stderr, which panics when stderr is a
    // terminal gone.
    std::mem::forget(terminal);
    restored
}

/// What the terminal answered, a terminal that is gone counting as done: there is nothing left to draw on or give
/// back.
fn unless_gone(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if output::gone(&error) => Ok(()),
        result => result,
    }
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
    // An activity read that fails leaves the health figures out rather than the whole screen.
    let activity = activity(client).await.ok();
    let mut app = App::new(live, activity, host.samples, series);
    app.update(Msg::Attention(attention(client).await));
    Ok((events, app))
}

/// The jobs started and finished in the last day.
async fn activity(client: &Client) -> Result<Activity, ClientError> {
    client
        .get_query(
            "/v1/activity",
            &ActivityQuery {
                since: Some(now().saturating_sub(DAY)),
            },
        )
        .await
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
fn suspend(terminal: &mut ratatui::DefaultTerminal, mouse: Mouse) -> std::io::Result<()> {
    output::restore()?;
    // The process group, as the terminal's own Ctrl-Z would; SIGSTOP, because SIGTSTP now comes back here.
    rustix::process::kill_current_process_group(rustix::process::Signal::STOP)?;
    resume(terminal, mouse)
}

/// Takes the terminal again and draws all of it at the next frame, since the shell or another program drew over it.
fn resume(terminal: &mut ratatui::DefaultTerminal, mouse: Mouse) -> std::io::Result<()> {
    *terminal = output::resume(mouse)?;
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

/// Reads keys on a thread of its own, since the terminal's reads block, and waits on a second one for that terminal
/// to go away (hung up, or its other end closed). Either thread ending ends the TUI: no keys means no `q`. The wait
/// is apart from the reads because crossterm, once it has read part of a sequence such as `ESC [`, reads a terminal
/// that is gone again and again without returning.
struct Keys {
    stop: Arc<AtomicBool>,
    /// Never sent, like `watched`: each resolves when its thread drops the sender, however the thread ends.
    read: oneshot::Receiver<Infallible>,
    watched: oneshot::Receiver<Infallible>,
}

impl Keys {
    fn spawn(send: UnboundedSender<Msg>, hits: Arc<Mutex<Hits>>) -> Keys {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (reading, read) = oneshot::channel();
        std::thread::spawn(move || {
            let _reading = reading;
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
                    Ok(TerminalEvent::Mouse(event)) => match clicked(event, &hits) {
                        Some(msg) => msg,
                        None => continue,
                    },
                    Ok(_) => continue,
                    Err(_) => return,
                };
                if send.send(msg).is_err() {
                    return;
                }
            }
        });
        let (watching, watched) = oneshot::channel();
        std::thread::spawn(move || {
            let _watching = watching;
            if let Ok(input) = keyboard() {
                wait_gone(&input);
            }
        });
        Keys {
            stop,
            read,
            watched,
        }
    }

    /// Once either thread ended; cancel-safe.
    async fn ended(&mut self) {
        tokio::select! {
            _ = &mut self.read => {}
            _ = &mut self.watched => {}
        }
    }

    /// Tells the reader to stop and does not wait for it: the process exits next, which ends a read in progress.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What a mouse event asks: a left click on what the last frame drew there, or a turn of the wheel.
fn clicked(event: MouseEvent, hits: &Mutex<Hits>) -> Option<Msg> {
    /// Rows a turn of the wheel moves.
    const WHEEL: isize = 3;
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => hits
            .lock()
            .ok()?
            .at(event.column, event.row)
            .map(Msg::Click),
        MouseEventKind::ScrollDown => Some(Msg::Wheel(WHEEL)),
        MouseEventKind::ScrollUp => Some(Msg::Wheel(-WHEEL)),
        _ => None,
    }
}

/// The terminal crossterm reads keys from: stdin when it is one, else `/dev/tty`.
fn keyboard() -> std::io::Result<OwnedFd> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return stdin.as_fd().try_clone_to_owned();
    }
    let tty = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    Ok(tty.into())
}

/// Returns once `input` is gone. It asks for no events, so a key does not wake it; the kernel reports a hangup, an
/// error and a closed descriptor whatever was asked.
fn wait_gone(input: &OwnedFd) {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut fds = [PollFd::new(input, PollFlags::empty())];
    while poll(&mut fds, None) == Err(rustix::io::Errno::INTR) {}
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
            Effect::RefreshJob(_)
            | Effect::Outline(_)
            | Effect::Lines { .. }
            | Effect::Search { .. }
            | Effect::Compare(_)
            | Effect::Act { .. }
            | Effect::History { .. }
            | Effect::Timeline { .. }
            | Effect::Runs { .. } => {
                tokio::spawn(jobs_request(client, send, effect));
            }
            effect => {
                tokio::spawn(daemon_request(client, send, effect));
            }
        }
    }

    fn close_log(&mut self) {
        if let Some(log) = self.log.take() {
            log.abort();
        }
    }
}

/// Why a read gave nothing: a daemon without the route, or the error.
fn unread(error: ClientError) -> Unread {
    if error.is_unsupported() {
        Unread::Unsupported
    } else {
        match error {
            ClientError::Api { message, .. } => Unread::Failed(message),
            error => Unread::Failed(error.to_string()),
        }
    }
}

/// What needs attention; a daemon without the route says it is not supported.
async fn attention(client: &Client) -> Result<api::Attention, Unread> {
    match client.attention().await {
        Ok(Some(attention)) => Ok(attention),
        Ok(None) => Err(Unread::Unsupported),
        Err(error) => Err(unread(error)),
    }
}

/// A request about jobs: one job's page, or the jobs list and timeline.
async fn jobs_request(client: Client, send: UnboundedSender<Msg>, effect: Effect) {
    match effect {
        Effect::RefreshJob(id) => {
            answer(
                &send,
                client.get::<Job>(&format!("/v1/jobs/{id}")).await,
                Msg::Job,
            );
        }
        Effect::Outline(job) => {
            let outline = client.log_outline(job).await.map_err(unread);
            let _ = send.send(Msg::Outline { job, outline });
        }
        Effect::Lines { job, range } => read_lines(client, send, job, range).await,
        Effect::Search { job, query } => {
            let found = client.search_log(job, &query).await.map_err(unread);
            let _ = send.send(Msg::Found {
                job,
                query: query.q,
                found,
            });
        }
        Effect::Compare(job) => {
            let comparison = client
                .compare(job, &CompareQuery::default())
                .await
                .map_err(unread);
            let _ = send.send(Msg::Compared { job, comparison });
        }
        Effect::Act { job, action, query } => {
            let _ = send.send(match client.job_action(job, action, &query).await {
                Ok(JobActionAnswer::Confirm(preview)) => Msg::Preview(preview),
                Ok(JobActionAnswer::Done(result)) => Msg::Acted(result),
                Err(error) if error.is_unsupported() => Msg::ActionFailed {
                    job,
                    why: api::JobActions::Unsupported
                        .why_not()
                        .unwrap_or_default()
                        .to_owned(),
                },
                Err(error) => Msg::ActionFailed {
                    job,
                    why: error.to_string(),
                },
            });
        }
        Effect::History {
            generation,
            query,
            range,
        } => {
            let query = JobQuery {
                since: range.since(now()),
                ..query
            };
            read_jobs(client, send, query, move |list| Msg::History {
                generation,
                list,
            })
            .await;
        }
        Effect::Timeline { generation, range } => {
            let timeline = client
                .timeline(&range.timeline(now()))
                .await
                .map_err(unread);
            let _ = send.send(Msg::Timeline {
                generation,
                timeline,
            });
        }
        Effect::Runs { job, query } => {
            read_jobs(client, send, query, move |list| Msg::Runs { job, list }).await;
        }
        _ => {}
    }
}

/// A request about the daemon: pausing, records, sizing, caches, the doctor, the config and what is read on a pace.
async fn daemon_request(client: Client, send: UnboundedSender<Msg>, effect: Effect) {
    match effect {
        Effect::Pause { repository, force } => {
            let _ = send.send(pause(&client, repository, force).await);
        }
        Effect::Resume { repository } => {
            let _ = send.send(unpause(&client, repository).await);
        }
        Effect::Records => answer(&send, client.get("/v1/records").await, Msg::Records),
        Effect::RemoveRecords(removal) => {
            let path = match removal {
                Removal::Job(job) => format!("/v1/records/{job}"),
                Removal::All => "/v1/records?all=true".to_owned(),
            };
            let removed = crate::remove_records(&client, &path).await;
            let _ = send.send(match removed {
                Ok(removed) => Msg::Done(removed.line()),
                Err(error) => Msg::Failed(error.to_string()),
            });
            answer(
                &send,
                client.get::<Records>("/v1/records").await,
                Msg::Records,
            );
        }
        Effect::Sizing => answer(&send, client.get::<Sizing>("/v1/sizing").await, Msg::Sizing),
        Effect::Caches => read_caches(client, send).await,
        Effect::Empty(volume) => {
            let emptied = client
                .post(&format!("/v1/caches/{volume}/clean"), &())
                .await;
            let _ = send.send(match emptied {
                Ok(_) => Msg::Done(format!("{volume} emptied")),
                Err(error) => Msg::Failed(error.to_string()),
            });
            read_caches(client, send).await;
        }
        Effect::Doctor => {
            let doctor = client.get::<Doctor>("/v1/doctor").await.map_err(unread);
            let _ = send.send(Msg::Doctor(doctor));
        }
        Effect::Config => {
            let _ = send.send(Msg::Config(read_config(&client).await));
        }
        Effect::Attention => {
            let _ = send.send(Msg::Attention(attention(&client).await));
        }
        Effect::Activity { generation } => read_activity(client, send, generation).await,
        Effect::Insights => read_insights(client, send).await,
        Effect::Reasons => {
            let status = client.get::<Status>("/v1/status").await.ok();
            let _ = send.send(Msg::Reasons(status.map(Box::new)));
        }
        Effect::Resize(resizing) => write_resize(client, send, resizing).await,
        Effect::Reseed { profile } => {
            let requested = client
                .post(&format!("/v1/profiles/{profile}/reseed"), &())
                .await;
            status(&client, &send).await;
            let _ = send.send(match requested {
                Ok(_) => Msg::Done(api::reseed_requested(&profile)),
                Err(error) => Msg::Failed(error.to_string()),
            });
        }
        _ => {}
    }
}

async fn read_caches(client: Client, send: UnboundedSender<Msg>) {
    let caches = client.get::<Caches>("/v1/caches").await.map_err(unread);
    let _ = send.send(Msg::Caches(caches));
}

/// The daemon's config and its check, as `runnerctl config check` reads them.
async fn read_config(client: &Client) -> Result<ConfigRead, Unread> {
    let file: ConfigFile = client.get("/v1/config").await.map_err(unread)?;
    let check: ConfigCheck = client
        .post_toml("/v1/config/check", file.text.clone())
        .await
        .map_err(unread)?;
    Ok(ConfigRead { file, check })
}

/// Reads the day's activity again. Like the snapshot, a daemon without it keeps the figures it has: no flash every
/// minute.
async fn read_activity(client: Client, send: UnboundedSender<Msg>, generation: u64) {
    if let Ok(activity) = activity(&client).await {
        let _ = send.send(Msg::Activity {
            generation,
            activity,
        });
    }
}

/// Reads the insights and what the jobs' minutes would have cost, and sends them; a read that fails sends nothing,
/// and the last one read stays shown.
async fn read_insights(client: Client, send: UnboundedSender<Msg>) {
    let insights = client
        .get_query::<Insights>("/v1/insights", &InsightsQuery::default())
        .await
        .map(Box::new)
        .map_err(unread);
    let _ = send.send(Msg::Insights(insights));
    if let Ok(savings) = client.get::<Savings>("/v1/savings").await {
        let _ = send.send(Msg::Savings(Box::new(savings)));
    }
}

/// Reads a list of jobs and sends it, or why it could not be read, as `msg` makes it.
async fn read_jobs(
    client: Client,
    send: UnboundedSender<Msg>,
    query: JobQuery,
    msg: impl FnOnce(Result<JobList, String>) -> Msg,
) {
    let list = client.jobs(&query).await.map_err(|error| error.to_string());
    let _ = send.send(msg(list));
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
        Ok(_) => Msg::Done(resize.written_line(&profile)),
        Err(error) => Msg::Failed(error.to_string()),
    });
    answer(&send, client.get::<Sizing>("/v1/sizing").await, Msg::Sizing);
}

/// Resumes `repository`, or every one when `None`.
async fn unpause(client: &Client, repository: Option<String>) -> Msg {
    let resumed = match crate::Target::new(repository) {
        Ok(target) => client
            .post(&format!("{}/resume", target.path), &())
            .await
            .map(|_| target.name)
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    match resumed {
        Ok(name) => Msg::Done(format!("{name} resumed")),
        Err(error) => Msg::Failed(error),
    }
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
        Ok(paused) => Msg::Done(paused.line(name)),
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

/// Reads lines `range` of a job's log, styled, and sends them as one batch.
async fn read_lines(client: Client, send: UnboundedSender<Msg>, job: u64, range: LineRange) {
    let query = LogLinesQuery {
        from: Some(range.from),
        limit: Some(u32::try_from(range.to.saturating_sub(range.from)).unwrap_or(u32::MAX)),
        follow: false,
    };
    let mut lines = Vec::new();
    let failed = match client.styled_lines(job, &query).await {
        Ok(mut read) => loop {
            match read.next().await {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        },
        Err(error) => Some(error),
    };
    let _ = send.send(Msg::Lines { job, lines });
    if let Some(error) = failed {
        // The range may be asked for again.
        let _ = send.send(Msg::LinesFailed {
            job,
            range,
            why: format!("log of job {job}: {error}"),
        });
    }
}

/// Reads the job, its samples, its log's outline and its last lines; follows the lines until the job's record is
/// final. A daemon without the outline gets the plain log, styled here by core's parser, under no outline.
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
    let outline = client.log_outline(id).await.map_err(unread);
    let plain = outline == Err(Unread::Unsupported);
    let _ = send.send(Msg::Outline { job: id, outline });
    let read = if plain {
        plain_lines(&client, &send, id, follow, from).await
    } else {
        styled_lines(&client, &send, id, follow, from).await
    };
    if let Err(error) = read {
        let _ = send.send(Msg::Failed(format!("log of job {id}: {error}")));
    }
}

/// The log's last lines, styled, then each line as it comes while `follow`.
async fn styled_lines(
    client: &Client,
    send: &UnboundedSender<Msg>,
    job: u64,
    follow: bool,
    from: u64,
) -> Result<(), ClientError> {
    let query = LogLinesQuery {
        from: Some(from),
        limit: Some(u32::try_from(LOG_LINES).unwrap_or(u32::MAX)),
        follow,
    };
    let mut read = client.styled_lines(job, &query).await?;
    // The lines stored come at once, then those that follow one by one: sent in batches as they come.
    let mut batch = Batch::new(Instant::now());
    loop {
        let next = if batch.lines.is_empty() {
            read.next().await?
        } else if let Some(wait) = batch.wait(Instant::now())
            && let Ok(next) = tokio::time::timeout(wait, read.next()).await
        {
            next?
        } else {
            if send
                .send(Msg::Lines {
                    job,
                    lines: batch.take(),
                })
                .is_err()
            {
                return Ok(());
            }
            continue;
        };
        match next {
            Some(line) => batch.push(line, Instant::now()),
            None => break,
        }
    }
    let _ = send.send(Msg::Lines {
        job,
        lines: batch.take(),
    });
    Ok(())
}

/// Lines a batch holds at most.
const BATCH_LINES: usize = 500;
/// The longest a line waits in a batch, through a burst that does not pause.
const BATCH_AGE: Duration = Duration::from_millis(100);
/// A pause this long sends the batch.
const BATCH_QUIET: Duration = Duration::from_millis(20);

/// Followed lines not sent yet, so the screen is drawn once per batch rather than once per line. Lives as long as one
/// read of the log.
#[derive(Debug)]
struct Batch {
    lines: Vec<StyledLine>,
    /// When its first line came.
    since: Instant,
}

impl Batch {
    fn new(now: Instant) -> Batch {
        Batch {
            lines: Vec::new(),
            since: now,
        }
    }

    fn push(&mut self, line: StyledLine, now: Instant) {
        if self.lines.is_empty() {
            self.since = now;
        }
        self.lines.push(line);
    }

    /// How long to wait for another line before it is sent: up to [`BATCH_QUIET`], and never past [`BATCH_AGE`]
    /// after its first line. `None` when it is due now, as when it holds [`BATCH_LINES`].
    fn wait(&self, now: Instant) -> Option<Duration> {
        if self.lines.len() >= BATCH_LINES {
            return None;
        }
        let age = now.saturating_duration_since(self.since);
        let wait = BATCH_QUIET.min(BATCH_AGE.saturating_sub(age));
        (!wait.is_zero()).then_some(wait)
    }

    fn take(&mut self) -> Vec<StyledLine> {
        std::mem::take(&mut self.lines)
    }
}

/// The plain log of a daemon without the styled lines, styled here by core's parser.
async fn plain_lines(
    client: &Client,
    send: &UnboundedSender<Msg>,
    job: u64,
    follow: bool,
    from: u64,
) -> Result<(), ClientError> {
    let mut read = client
        .logs(
            job,
            &LogQuery {
                follow,
                from: Some(from),
            },
        )
        .await?;
    let mut line = from;
    while let Some(text) = read.next().await? {
        let styled = StyledLine::parse(line, &text);
        line += 1;
        if send
            .send(Msg::Lines {
                job,
                lines: vec![styled],
            })
            .is_err()
        {
            break;
        }
    }
    Ok(())
}
