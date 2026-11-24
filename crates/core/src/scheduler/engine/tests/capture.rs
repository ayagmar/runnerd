//! A runner's log capture with the real capture script over a temporary `_diag` (ADR 0176): the runner removes pages
//! as it uploads them, the history fails writes after they were sent, mid-stream, at the script's end and at `log_max`,
//! and the daemon restarts; the stored log is exactly the runner's, every line once.

use std::path::{Path, PathBuf};

use super::*;
use crate::observe::capture::GAP;

/// The worker log of a runner that took job 1 of run 1, as job record `abc-1`.
const WORKER_LOG: &str = "\"k\": \"check_run_id\",\n\"v\": 1.0\n\"k\": \"run_id\",\n\"v\": \"1\"\n\"jobId\": \"abc-1\"\n";

/// A running runner in slot 1 whose capture runs the real script over `dir/_diag`, which it returns.
fn script_runner(github: &FakeGithub, docker: &FakeDocker, dir: &TestDir) -> PathBuf {
    running_runner(github, docker, "");
    let diag = dir.0.join("_diag");
    std::fs::create_dir_all(diag.join("pages")).unwrap();
    std::fs::write(diag.join("Worker_20261124.log"), WORKER_LOG).unwrap();
    docker.0.lock().unwrap().script_captures.insert(
        "container-1".to_owned(),
        ScriptCapture {
            diag: diag.clone(),
            end_after: None,
        },
    );
    diag
}

fn page(diag: &Path, page: u64) -> PathBuf {
    diag.join(format!("pages/tl_abc-1_{page}.log"))
}

fn kept(diag: &Path, page: u64) -> PathBuf {
    diag.join(format!(".runnerd-pages/tl_abc-1_{page}.log"))
}

/// The runner writes `text` at the end of a page.
fn append(path: &Path, text: &str) {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .unwrap();
}

/// Job 1's stored log, once it holds `count` lines; fails after 30 s.
async fn until_log(handle: &Handle, count: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        handle.store.flush().await;
        let lines: Vec<String> = handle
            .store
            .log_lines(1, 0, 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        if lines.len() >= count {
            return lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job 1 never stored {count} lines: {lines:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until `path` is gone; fails after 30 s.
async fn until_gone(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never went",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the engine's captures ended, by the wall clock, then lets `recapture` see how they ended.
async fn until_capture_ends(engine: &mut Engine<FakeGithub, FakeDocker>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !engine.watches.every_capture_ended() {
        assert!(
            std::time::Instant::now() < deadline,
            "the capture never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The output of container 1's next captures ends right after the script prints `text`.
fn end_script_after(docker: &FakeDocker, text: Option<&str>) {
    docker
        .0
        .lock()
        .unwrap()
        .script_captures
        .get_mut("container-1")
        .unwrap()
        .end_after = text.map(str::to_owned);
}

/// The history fails to store the log line `text`, as a full disk would, from now until `heal`.
struct Fault(rusqlite::Connection);

impl Fault {
    fn on(dir: &TestDir, text: &str) -> Fault {
        let db = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
        db.execute_batch(&format!(
            "CREATE TRIGGER no_line BEFORE INSERT ON log_lines WHEN NEW.text = '{text}' \
             BEGIN SELECT RAISE(ABORT, 'full'); END;"
        ))
        .unwrap();
        Fault(db)
    }

    fn heal(self) {
        self.0.execute_batch("DROP TRIGGER no_line").unwrap();
    }
}

/// The capture starts again once a broken one waited out its backoff: at once after the first break, 5 s after the
/// second.
async fn recapture_after(engine: &mut Engine<FakeGithub, FakeDocker>, wait: Duration) {
    // The tick that notes the break starts its wait.
    engine.tick(Shutdown::No).await;
    if !wait.is_zero() {
        tokio::time::pause();
        tokio::time::advance(wait).await;
        engine.tick(Shutdown::No).await;
        tokio::time::resume();
    }
}

// Catches: a page dropped once the runner removed it, before the history committed its lines, so a capture started
// again after a failed write finds nothing; a resume from where lines were sent rather than committed, skipping the
// failed ones; a failed segment's successors stored past it; a capture done at the script's end while its last
// lines wait for the commit that then fails; and a restart that cannot find the pages its predecessor read.
#[tokio::test]
async fn a_capture_keeps_each_page_until_its_lines_commit_through_failed_writes_its_end_and_a_restart()
 {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("capture-script");
    let diag = script_runner(&github, &docker, &dir);
    append(&page(&diag, 1), "a1\na2\n");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    assert_eq!(until_log(&handle, 2).await, ["a1", "a2"]);

    // The history fails "b1" after the capture sent it; the runner uploads page 1, removes it, and starts page 2,
    // whose line the capture sends after the failed one.
    let fault = Fault::on(&dir, "b1");
    append(&page(&diag, 1), "b1\nb2\n");
    tokio::time::sleep(Duration::from_millis(600)).await;
    std::fs::remove_file(page(&diag, 1)).unwrap();
    append(&page(&diag, 2), "c1\n");
    until_capture_ends(&mut engine).await;
    handle.store.flush().await;
    assert_eq!(
        until_log(&handle, 2).await,
        ["a1", "a2"],
        "nothing is stored past the failed line"
    );
    assert!(
        kept(&diag, 1).exists(),
        "page 1 is kept: the history has not committed it"
    );

    // Started again from where the history's log ends, it reads the rest of page 1 from the page it kept. Its output
    // will end right after "c2", as when the container stops then.
    end_script_after(&docker, Some("c2"));
    fault.heal();
    recapture_after(&mut engine, Duration::ZERO).await;
    assert_eq!(until_log(&handle, 5).await, ["a1", "a2", "b1", "b2", "c1"]);
    until_gone(&kept(&diag, 1)).await;
    let starts: Vec<(String, String)> = docker
        .0
        .lock()
        .unwrap()
        .captured
        .iter()
        .map(|(_, page, offset)| (page.clone(), offset.clone()))
        .collect();
    assert_eq!(
        starts,
        [
            ("1".to_owned(), "0".to_owned()),
            ("1".to_owned(), "6".to_owned())
        ],
        "the second capture starts after the committed lines"
    );

    // Its output ends right after "c2", while "c2" waits for a commit that fails: the capture is not done.
    let fault = Fault::on(&dir, "c2");
    append(&page(&diag, 2), "c2\n");
    until_capture_ends(&mut engine).await;
    // The write failed before the fault heals, however soon the capture ended.
    handle.store.flush().await;
    fault.heal();
    end_script_after(&docker, None);
    recapture_after(&mut engine, Duration::from_secs(5)).await;
    assert_eq!(
        until_log(&handle, 6).await,
        ["a1", "a2", "b1", "b2", "c1", "c2"]
    );

    // The daemon restarts; meanwhile the runner finishes page 2 and removes it. The restarted capture reads the page
    // its predecessor kept.
    engine.watches.abort_all();
    drop(engine);
    drop(handle);
    append(&page(&diag, 2), "d1\n");
    std::fs::remove_file(page(&diag, 2)).unwrap();
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    let log = until_log(&handle, 7).await;
    assert_eq!(log, ["a1", "a2", "b1", "b2", "c1", "c2", "d1"]);
    until_gone(&kept(&diag, 2)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(until_log(&handle, 7).await, log, "no line is stored twice");
    assert!(!log.iter().any(|line| line == GAP), "nothing was lost");
}

// Catches: a capture that ends done at `log_max` while the lines that reached it wait for a commit that fails, which
// leaves the log short of its cap for good, and a capture that reads on past the cap.
#[tokio::test]
async fn a_capture_at_log_max_is_done_only_once_the_lines_up_to_it_commit() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("capture-cap");
    let diag = script_runner(&github, &docker, &dir);
    let config = CONFIG.replace("[host]", "[history]\nlog_max = \"0.009K\"\n\n[host]");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    append(&page(&diag, 1), "a1\n");
    engine.recover().await.unwrap();
    assert_eq!(until_log(&handle, 1).await, ["a1"]);
    let fault = Fault::on(&dir, "a2");
    append(&page(&diag, 1), "a2\na3\na4\n");
    until_capture_ends(&mut engine).await;
    let job = handle.store.job(1).await.unwrap().unwrap().summary;
    assert!(!job.log.truncated, "the cut was not stored");
    fault.heal();
    recapture_after(&mut engine, Duration::ZERO).await;
    until_capture_ends(&mut engine).await;
    let job = handle.store.job(1).await.unwrap().unwrap().summary;
    assert!(job.log.truncated, "job 1's log is cut at log_max");
    assert_eq!(until_log(&handle, 3).await, ["a1", "a2", "a3"]);
    recapture_after(&mut engine, Duration::from_mins(5)).await;
    assert_eq!(
        docker.0.lock().unwrap().captured.len(),
        2,
        "a capture done at log_max is not started again"
    );
}

// Catches: a restarted daemon that silently stitches over pages the runner removed while no capture ran, rather than
// saying lines are missing there.
#[tokio::test]
async fn a_capture_names_the_lines_the_runner_removed_before_any_capture_kept_them() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("capture-gap");
    let diag = script_runner(&github, &docker, &dir);
    append(&page(&diag, 1), "a1\n");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    assert_eq!(until_log(&handle, 1).await, ["a1"]);
    engine.watches.abort_all();
    drop(engine);
    drop(handle);
    // While no daemon runs, the rest of page 1 is lost with its link, as when the container's disk lost it: the
    // runner uploaded and removed page 1 and wrote page 2.
    append(&page(&diag, 1), "a2\n");
    std::fs::remove_file(kept(&diag, 1)).unwrap();
    std::fs::remove_file(page(&diag, 1)).unwrap();
    append(&page(&diag, 2), "c1\n");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    assert_eq!(until_log(&handle, 3).await, ["a1", GAP, "c1"]);
}

// Catches: acks that break the history's group commit, one transaction per runner's segment, as a commit that waited
// for its capture's answer before the next would.
#[tokio::test]
async fn the_captures_of_several_runners_still_commit_together() {
    let dir = TestDir::new("capture-batch");
    let store = Store::open(&dir.0.join("history.db")).unwrap();
    for job in 1..=4 {
        store.queued("o/r", &super::job(job), 1);
    }
    store.flush().await;
    let wal = dir.0.join("history.db-wal");
    let before = commits(&wal);
    // Four captures send a segment each, twice, as noisy runners do every 200 ms.
    let mut answers = Vec::new();
    for round in 0..2_u64 {
        for job in 1..=4 {
            let from = crate::store::LogPosition {
                page: 1,
                offset: round * 3,
            };
            answers.push(
                store
                    .append_log(crate::store::LogSegment {
                        job,
                        lines: vec![format!("l{round}")],
                        from,
                        to: crate::store::LogPosition {
                            page: 1,
                            offset: from.offset + 3,
                        },
                        max_bytes: u64::MAX,
                    })
                    .await,
            );
        }
    }
    for answer in answers {
        assert_eq!(answer.await.unwrap(), crate::store::Appended::Stored);
    }
    assert_eq!(
        commits(&wal) - before,
        1,
        "eight segments of four captures commit in one transaction"
    );
    for job in 1..=4 {
        let lines = store.log_lines(job, 0, 10).await.unwrap();
        assert_eq!(lines, [(0, "l0".to_owned()), (1, "l1".to_owned())]);
    }
}

/// Transactions committed to the WAL at `wal` since it was last reset: frames that end a commit, which hold the
/// database's size, with the salts of the WAL's header.
fn commits(wal: &Path) -> usize {
    let wal = std::fs::read(wal).unwrap();
    let page_size = usize::try_from(u32::from_be_bytes(wal[8..12].try_into().unwrap())).unwrap();
    wal[32..]
        .chunks(24 + page_size)
        .take_while(|frame| frame.len() == 24 + page_size && frame[8..16] == wal[16..24])
        .filter(|frame| frame[4..8] != [0; 4])
        .count()
}
