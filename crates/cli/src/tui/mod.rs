//! `runnerctl tui`: a full-screen dashboard of the daemon, for hosts reached over SSH. A pure API client like the
//! rest of `runnerctl`: a snapshot from `/v1/*`, then `/v1/events`. This module runs the loop: it feeds keys, events
//! and answers to `app`'s reducer, makes the requests the reducer asks for, and draws with `draw` after each batch
//! and every second. It decides nothing itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::event::{self as terminal, Event as TerminalEvent};
use runnerd_core::api::{
    EventQuery, HostSampleQuery, HostSamples, Job, JobPage, PauseQuery, Paused, SampleSeries,
    Status,
};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::client::{Client, ClientError, Events};
use crate::{Error, now, output, watch};

mod app;
mod draw;
#[cfg(test)]
mod tests;

use app::{App, Effect, LOG_LINES, Msg};
use draw::Theme;

/// How long the feed waits between attempts to reach a daemon that went away.
const RETRY: Duration = Duration::from_secs(2);
/// How far back the slice's charts start.
const HOST_HISTORY: u64 = 30 * 60;

/// Reads the snapshot, takes over the terminal and runs until `q`. The terminal is given back however it ends.
pub(crate) async fn run(client: &Client, colour: bool) -> Result<(), Error> {
    // The stream opens first, so nothing that happens while the snapshot is read is missed.
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
    let mut app = App::new(live, host.samples, series);

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
    let ended = loop {
        if let Err(error) = terminal.draw(|frame| draw::draw(frame, &app, now(), theme)) {
            break Err(local(&error));
        }
        let msg = tokio::select! {
            msg = receive.recv() => msg,
            _ = tick.tick() => continue,
        };
        let Some(mut msg) = msg else {
            break Ok(());
        };
        // Everything already waiting is applied before the next frame.
        let quit = loop {
            if app
                .update(msg)
                .into_iter()
                .any(|effect| !requests.make(effect))
            {
                break true;
            }
            match receive.try_recv() {
                Ok(next) => msg = next,
                Err(_) => break false,
            }
        };
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

fn local(error: &std::io::Error) -> Error {
    Error::Local(format!("terminal: {error}"))
}

/// Reads events into the loop. When the stream fails for good (the daemon stopped), it says so and tries again
/// every `retry`. With an event read before, the new stream resumes after it and replays what was missed. Without
/// one there is nothing to replay from, so it reads the state again once the new stream is open. It owns the
/// stream, so a read is never dropped half-way and the stream's idle timeout is not restarted by redraws.
async fn feed(client: Client, mut events: Events, send: UnboundedSender<Msg>, retry: Duration) {
    let mut last = None;
    loop {
        match events.next().await {
            Ok(envelope) => {
                if envelope.id.is_some() {
                    last = envelope.id;
                }
                if send.send(Msg::Event(envelope)).is_err() {
                    return;
                }
            }
            Err(error) => {
                if send.send(Msg::Disconnected(short(&error))).is_err() {
                    return;
                }
                events = loop {
                    tokio::time::sleep(retry).await;
                    let reopened = match client.events(EventQuery::default(), last).await {
                        Ok(events) if last.is_some() => Ok((events, None)),
                        // Opened before the state is read, so nothing that happens meanwhile is missed.
                        Ok(events) => watch::State::read(&client)
                            .await
                            .map(|state| (events, Some(state))),
                        Err(error) => Err(error),
                    };
                    match reopened {
                        Ok((events, state)) => {
                            if let Some(state) = state
                                && send.send(Msg::Snapshot(Box::new(state))).is_err()
                            {
                                return;
                            }
                            break events;
                        }
                        Err(error) => {
                            if send.send(Msg::Disconnected(short(&error))).is_err() {
                                return;
                            }
                        }
                    }
                };
                if send.send(Msg::Reconnected).is_err() {
                    return;
                }
            }
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
    /// Starts the request; false to quit.
    fn make(&mut self, effect: Effect) -> bool {
        let client = self.client.clone();
        let send = self.send.clone();
        match effect {
            Effect::Quit => return false,
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
                    let msg = pause(&client, &repository, force).await;
                    status(&client, &send).await;
                    let _ = send.send(msg);
                });
            }
            Effect::Resume { repository } => {
                tokio::spawn(async move {
                    let resumed = match crate::repository_path(&repository) {
                        Ok(path) => client
                            .post(&format!("{path}/resume"), &())
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    status(&client, &send).await;
                    let _ = send.send(match resumed {
                        Ok(_) => Msg::Done(format!("{repository} resumed")),
                        Err(error) => Msg::Failed(error),
                    });
                });
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
        true
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

/// Repositories' paused flags come with the status only, so it is read again after a pause or resume.
async fn status(client: &Client, send: &UnboundedSender<Msg>) {
    answer(send, client.get::<Status>("/v1/status").await, Msg::Status);
}

async fn pause(client: &Client, repository: &str, force: bool) -> Msg {
    let path = match crate::repository_path(repository) {
        Ok(path) => format!("{path}/pause"),
        Err(error) => return Msg::Failed(error.to_string()),
    };
    match client
        .post_json::<Paused>(&path, &PauseQuery { force })
        .await
    {
        Ok(paused) if paused.stopped.is_empty() => {
            Msg::Done(format!("{repository} paused: it gets no new runners"))
        }
        Ok(paused) => Msg::Done(format!(
            "{repository} paused; stopped {}",
            paused
                .stopped
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(ClientError::Api { status, message }) if status == hyper::StatusCode::CONFLICT => {
            Msg::Busy {
                repository: repository.to_owned(),
                message,
            }
        }
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
