//! What the engine remembers of a registration whose slot is gone, through the ticks that write and end it.

use super::*;
use crate::scheduler::SlotRef;

/// The runner `idle_runner` adopts.
const RUNNER: u64 = 7;

/// A registration of this machine that no slot holds.
const ORPHAN: u64 = 9;

/// An engine that adopted `idle_runner` and has seen one listing.
async fn adopted(
    github: &FakeGithub,
    docker: &FakeDocker,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    idle_runner(github, docker);
    let (mut engine, handle, dir) = engine(github, docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    (engine, handle, dir)
}

/// The runner's container exits and GitHub refuses both deletes of its registration this tick makes.
async fn released_while_github_holds_it(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    docker: &FakeDocker,
) {
    github.0.lock().unwrap().busy_deletes = 2;
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
}

/// The messages the engine logs on this thread, while installed with `tracing::subscriber::set_default`.
#[derive(Clone, Default)]
struct LogLines(Arc<Mutex<Vec<String>>>);

impl LogLines {
    fn containing(&self, text: &str) -> usize {
        let lines = self.0.lock().unwrap();
        lines.iter().filter(|line| line.contains(text)).count()
    }
}

impl tracing::Subscriber for LogLines {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Message(&self.0));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Installs `lines` on this thread. A second live dispatcher is held because with only one, `tracing-core` computes a
/// callsite's interest from the thread that reaches it first, and a parallel test with no subscriber caches "never".
fn logging(lines: &LogLines) -> tracing::subscriber::DefaultGuard {
    static SECOND: std::sync::LazyLock<tracing::Dispatch> =
        std::sync::LazyLock::new(|| tracing::Dispatch::new(LogLines::default()));
    std::sync::LazyLock::force(&SECOND);
    tracing::subscriber::set_default(lines.clone())
}

/// Takes an event's message into the lines it borrows.
struct Message<'lines>(&'lines Mutex<Vec<String>>);

impl tracing::field::Visit for Message<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.lock().unwrap().push(format!("{value:?}"));
        }
    }
}

/// Lists a busy registration of this machine that no slot holds, and has GitHub refuse the next `refusals` deletes.
fn orphan_listed(github: &FakeGithub, refusals: u32) {
    let mut state = github.0.lock().unwrap();
    state.runners.push(runner(
        ORPHAN,
        &identity("ci", ID_A).runner_name("app", 2, 1),
        true,
    ));
    state.registered_on.insert(ORPHAN, "o/r".to_owned());
    state.busy_deletes = refusals;
}

fn app_slot() -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index: 0,
    }
}

#[tokio::test]
async fn a_released_runner_github_holds_is_known_as_ended_until_a_listing_no_longer_has_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    assert!(!engine.ended_here(RUNNER), "it still has its slot");

    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "released here, and still listed");
    assert_eq!(
        engine.state_counts().busy_orphans,
        0,
        "GitHub refusing a runner ended here is no orphan"
    );

    engine.tick(Shutdown::No).await;
    assert_eq!(github.0.lock().unwrap().runners.len(), 0, "GitHub let go");
    assert!(
        engine.ended_here(RUNNER),
        "the listing that deleted it still had it"
    );

    engine.tick(Shutdown::No).await;
    assert!(
        !engine.ended_here(RUNNER),
        "the next listing has no such runner"
    );
}

#[tokio::test]
async fn a_killed_runner_counts_as_ended_only_if_github_keeps_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    github.0.lock().unwrap().busy_deletes = 1;
    engine.kill(&app_slot()).await;
    assert!(engine.ended_here(RUNNER), "GitHub refused the delete");
    engine.tick(Shutdown::No).await;
    assert!(
        engine.ended_here(RUNNER),
        "the listing that deleted it still had it"
    );
    engine.tick(Shutdown::No).await;
    assert!(
        !engine.ended_here(RUNNER),
        "the next listing no longer has it"
    );

    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    engine.kill(&app_slot()).await;
    assert!(!engine.ended_here(RUNNER), "GitHub let it go at once");
}

#[tokio::test]
async fn a_registration_no_slot_explains_is_recorded_as_refused_and_not_as_ended_here() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    orphan_listed(&github, 1);
    engine.tick(Shutdown::No).await;
    assert_eq!(engine.state_counts().busy_orphans, 1, "refused once");
    assert!(!engine.ended_here(ORPHAN), "this engine never ran it");
}

#[tokio::test]
async fn an_orphan_github_holds_is_warned_of_at_its_first_refusal_and_never_again() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    let lines = LogLines::default();
    let _logging = logging(&lines);
    orphan_listed(&github, 2);

    engine.tick(Shutdown::No).await;
    assert_eq!(lines.containing("GitHub says it runs a job"), 1, "first");

    engine.tick(Shutdown::No).await;
    assert_eq!(lines.containing("GitHub says it runs a job"), 1, "second");
}

#[tokio::test]
async fn deleting_a_registration_github_held_past_a_release_raises_no_false_alarm() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    let lines = LogLines::default();
    let _logging = logging(&lines);
    released_while_github_holds_it(&mut engine, &github, &docker).await;

    engine.tick(Shutdown::No).await;
    assert_eq!(github.0.lock().unwrap().runners.len(), 0, "GitHub let go");
    assert_eq!(lines.containing("which had no container"), 0);
    assert_eq!(lines.containing("GitHub says it runs a job"), 0);
}

#[tokio::test]
async fn a_failed_listing_says_nothing_of_a_runner_ended_here() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    github.0.lock().unwrap().runners.clear();

    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    engine.tick(Shutdown::No).await;
    assert!(engine.ended_here(RUNNER), "GitHub did not say it is gone");

    github.0.lock().unwrap().failed_runners.clear();
    engine.tick(Shutdown::No).await;
    assert!(!engine.ended_here(RUNNER), "this listing does");
}

#[tokio::test]
async fn a_runner_of_a_removed_repository_is_known_as_ended_while_its_repository_drains() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    engine.reload(
        Config::parse(&CONFIG.replace("name = \"o/r\"", "name = \"o/x\"")).unwrap(),
        None,
        None,
    );
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "the drain still lists it");

    github.0.lock().unwrap().runners.clear();
    engine.tick(Shutdown::No).await;
    assert!(!engine.ended_here(RUNNER), "the drain's listing ends it");
}

#[tokio::test]
async fn a_restarted_engine_knows_of_no_runner_it_ended() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("registrations");
    idle_runner(&github, &docker);
    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "known before the restart");
    drop(engine);

    let (mut restarted, _handle) = engine_in(&github, &docker, &dir);
    restarted.recover().await.unwrap();
    assert!(!restarted.ended_here(RUNNER), "nothing survives a restart");
    assert_eq!(restarted.state_counts().busy_orphans, 0);
}

/// Reloads `engine` without the repository `o/r`, which starts its drain.
fn remove_repository(engine: &mut Engine<FakeGithub, FakeDocker>) {
    engine.reload(
        Config::parse(&CONFIG.replace("name = \"o/r\"", "name = \"o/x\"")).unwrap(),
        None,
        None,
    );
}

#[tokio::test]
async fn an_orphan_github_held_is_forgotten_once_a_listing_no_longer_has_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    orphan_listed(&github, 1);
    engine.tick(Shutdown::No).await;
    assert_eq!(engine.state_counts().busy_orphans, 1, "GitHub holds it");

    github
        .0
        .lock()
        .unwrap()
        .runners
        .retain(|runner| runner.id != ORPHAN);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.state_counts().busy_orphans,
        0,
        "the listing no longer has it"
    );
}

#[tokio::test]
async fn a_runner_ended_here_is_forgotten_when_the_drain_of_its_repository_gives_up() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    remove_repository(&mut engine);
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "its drain still lists it");

    // The token lost the repository: no listing of it will ever say the runner went.
    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::NOT_FOUND);
    engine.tick(Shutdown::No).await;
    assert!(!engine.ended_here(RUNNER), "nothing lists its repository");
}

#[tokio::test]
async fn the_claims_of_a_removed_repository_last_as_long_as_its_drain() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    remove_repository(&mut engine);
    engine.spend("o/r", "app");
    engine.consume("o/r", 5);

    engine.tick(Shutdown::No).await;
    assert!(engine.has_claims("o/r"), "one clean listing ends no drain");

    engine.tick(Shutdown::No).await;
    assert!(!engine.has_claims("o/r"), "nothing lists it any more");
}

#[tokio::test]
async fn the_claims_of_a_repository_the_config_keeps_stay_through_a_failed_listing() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.consume("o/r", 5);
    github.0.lock().unwrap().rate_limited =
        Some(std::time::SystemTime::now() + Duration::from_secs(60));
    engine.tick(Shutdown::No).await;
    assert!(engine.has_claims("o/r"), "no listing answered");
}
