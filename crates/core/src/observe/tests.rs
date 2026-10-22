//! Metrics from fixture cgroup and proc trees, the capture script's output, and mirror attribution. Never the
//! host's cgroups.

use std::path::Path;

use super::capture::{Captured, MAX_LINE, Parser};
use super::metrics::{Paths, Sampler, Tracked};
use super::mirror::{self, Leases};
use crate::store::{LOG_BACKLOG, LogPosition, MirrorPosition};
use crate::testdir::TestDir;

/// Writes a container's cgroup files under the `ci.slice` fixture, and its network counters.
fn container(paths: &Paths, id: &str, pid: u32, current: u64, inactive_file: u64, usage_usec: u64) {
    let cgroup = paths.container("ci.slice", id);
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(cgroup.join("memory.current"), format!("{current}\n")).unwrap();
    std::fs::write(
        cgroup.join("memory.stat"),
        format!("anon 100\nfile 9000\nactive_file 500\ninactive_file {inactive_file}\n"),
    )
    .unwrap();
    std::fs::write(
        cgroup.join("cpu.stat"),
        format!("usage_usec {usage_usec}\nuser_usec 1\nsystem_usec 1\n"),
    )
    .unwrap();
    let net = paths.proc.join(pid.to_string()).join("net");
    std::fs::create_dir_all(&net).unwrap();
    std::fs::write(
        net.join("dev"),
        "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
         lo:   10463      96    0    0    0     0          0         0    10463      96    0    0    0     0       0          0\n  \
         eth0:  346864     340    0    0    0     0          0         0   129381     370    0    0    0     0       0          0\n\
         docker0:      28       1    0    0    0     0          0         0      306       3    0    4    0     0       0          0\n",
    )
    .unwrap();
}

fn paths(dir: &Path) -> Paths {
    Paths {
        cgroup: dir.join("cgroup"),
        proc: dir.join("proc"),
    }
}

fn tracked(runner: &str, id: &str, pid: u32) -> Tracked {
    Tracked {
        runner: runner.to_owned(),
        container: id.to_owned(),
        pid,
    }
}

#[test]
fn a_nested_slice_maps_to_its_systemd_cgroup_path() {
    let paths = paths(Path::new("/c"));
    assert_eq!(
        paths.container("ci-runners.slice", "abc"),
        Path::new("/c/cgroup/ci.slice/ci-runners.slice/docker-abc.scope")
    );
}

#[test]
fn memory_leaves_out_the_page_cache_and_cpu_is_use_between_samples() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000_000, 1_500_000, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    let (first, _) = state.sample(&runners, 10_000);
    // Two seconds later the container used 3 CPU seconds: 1.5 cores.
    container(&paths, "a", 10, 5_000_000, 1_000_000, 4_000_000);
    let (second, _) = state.sample(&runners, 12_000);
    let observed: Vec<(u64, u64)> = first
        .iter()
        .chain(&second)
        .map(|sample| (sample.memory, sample.cpu))
        .collect();
    assert_eq!(observed, [(2_500_000, 0), (4_000_000, 1500)]);
    // eth0 only: not loopback, not the inner Docker Engine's bridge.
    assert_eq!(
        (second[0].network_received, second[0].network_sent),
        (346_864, 129_381)
    );
}

#[test]
fn a_container_that_disappears_ends_its_series_and_the_others_go_on() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 0);
    container(&paths, "b", 11, 8_000, 0, 0);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10), tracked("runner-b", "b", 11)];
    assert_eq!(state.sample(&runners, 1_000).0.len(), 2);
    std::fs::remove_dir_all(paths.container("ci.slice", "a")).unwrap();
    let (after, _) = state.sample(&runners, 3_000);
    let names: Vec<&str> = after.iter().map(|sample| sample.runner.as_str()).collect();
    assert_eq!(names, ["runner-b"]);
}

fn feed_all(chunks: &[&[u8]]) -> Vec<Captured> {
    let mut parser = Parser::default();
    chunks.iter().flat_map(|chunk| parser.feed(chunk)).collect()
}

fn lines(captured: &[Captured]) -> Vec<String> {
    captured
        .iter()
        .flat_map(|captured| match captured {
            Captured::Lines { lines, .. } => lines.clone(),
            Captured::Job { .. } => Vec::new(),
        })
        .collect()
}

#[test]
fn the_capture_yields_the_job_then_whole_lines_however_the_output_is_split() {
    let output = "@@job 120038758684 39840158500\n@@page 1 0 14\n\u{feff}first\nsecon@@page 1 14 7\nd\nthird@@end 1\n";
    let whole = feed_all(&[output.as_bytes()]);
    assert_eq!(
        whole[0],
        Captured::Job {
            id: 120_038_758_684,
            run_id: 39_840_158_500
        }
    );
    assert_eq!(lines(&whole), ["first", "second", "third"]);
    let bytes = output.as_bytes();
    for split in 1..bytes.len() {
        let parts = feed_all(&[&bytes[..split], &bytes[split..]]);
        assert_eq!(
            lines(&parts),
            ["first", "second", "third"],
            "split at {split}"
        );
    }
    // The last position is the next page, so a restart reads nothing twice.
    assert!(matches!(
        whole.last(),
        Some(Captured::Lines {
            position: LogPosition { page: 2, offset: 0 },
            ..
        })
    ));
}

#[test]
fn a_partial_line_is_not_stored_until_its_end_arrives() {
    let captured = feed_all(&[b"@@job 1 2\n@@page 1 0 9\nab\ncd\nef"]);
    assert_eq!(lines(&captured), ["ab", "cd"]);
    // Resuming at offset 6 sends "ef" again, with the rest of its line.
    assert!(matches!(
        captured.last(),
        Some(Captured::Lines {
            position: LogPosition { page: 1, offset: 6 },
            ..
        })
    ));
}

const MIRROR_LOG: &str = "\
2026-10-22T19:27:25.233122085Z time=\"2026-10-22T19:27:25Z\" level=info msg=\"response completed\"
2026-10-22T19:27:25.869343464Z 172.18.0.3 - - [22/Oct/2026:19:27:25 +0000] \"GET /v2/library/hello-world/blobs/sha256:e2 HTTP/1.1\" 200 577 \"\" \"docker/29.9.2\"
2026-10-22T19:27:26.891132173Z 172.18.0.3 - - [22/Oct/2026:19:27:26 +0000] \"GET /v2/library/hello-world/blobs/sha256:4f HTTP/1.1\" 200 2415 \"\" \"docker/29.9.2\"
2026-10-22T19:27:30.000000000Z 172.18.0.3 - - [22/Oct/2026:19:27:30 +0000] \"HEAD /v2/library/hello-world/manifests/latest HTTP/1.1\" 200 0 \"\" \"docker/29.9.2\"
2026-10-22T19:27:31.000000000Z 172.18.0.3 - - [22/Oct/2026:19:27:31 +0000] \"GET /v2/library/alpine/blobs/sha256:aa HTTP/1.1\" 200 1000 \"\" \"docker/29.9.2\"
";

#[test]
fn mirror_bytes_go_to_the_runner_that_held_the_address_at_the_time() {
    assert_eq!(mirror::parse(MIRROR_LOG.lines().next().unwrap()), None);
    let mut leases = Leases::default();
    // runner-1 had the address until 19:27:28; runner-2 got it at 19:27:29.
    leases.start("172.18.0.3", "runner-1", 1_792_697_240_000);
    leases.end("runner-1", 1_792_697_248_000);
    leases.start("172.18.0.3", "runner-2", 1_792_697_249_000);
    let (bytes, until) = leases.attribute(MIRROR_LOG, MirrorPosition::default());
    assert_eq!(
        bytes,
        [("runner-1".to_owned(), 2992), ("runner-2".to_owned(), 1000)]
    );
    assert_eq!(
        until,
        MirrorPosition {
            at: 1_792_697_251_000_000_000,
            seen: 1
        }
    );
    // Read again from there: nothing is counted twice.
    assert_eq!(leases.attribute(MIRROR_LOG, until).0, []);
}

#[test]
fn a_response_logged_in_the_same_millisecond_as_the_last_one_counted_is_counted_next_time() {
    let served = |at: &str, bytes: u64| {
        format!(
            "2026-10-22T19:27:25.{at}Z 172.18.0.3 - - [22/Oct/2026:19:27:25 +0000] \"GET /v2/x/blobs/sha256:{at} \
             HTTP/1.1\" 200 {bytes} \"\" \"docker/29.9.2\"\n"
        )
    };
    let mut leases = Leases::default();
    leases.start("172.18.0.3", "runner-1", 1_792_697_240_000);
    let first = served("869100000", 100) + &served("869100000", 200);
    let (bytes, until) = leases.attribute(&first, MirrorPosition::default());
    assert_eq!(bytes, [("runner-1".to_owned(), 300)]);
    // The next read starts at the same second and has two more responses: one logged at the very same time, one
    // later in the same millisecond.
    let second = first + &served("869100000", 400) + &served("869900000", 800);
    let (bytes, until) = leases.attribute(&second, until);
    assert_eq!(bytes, [("runner-1".to_owned(), 1200)]);
    assert_eq!(leases.attribute(&second, until).0, []);
}

#[test]
fn a_line_larger_than_the_backlog_and_the_log_cap_is_cut_as_it_arrives() {
    let total = 3 * usize::try_from(LOG_BACKLOG).unwrap();
    let mut parser = Parser::default();
    let mut captured = parser.feed(format!("@@page 1 0 {}\n", total + 1).as_bytes());
    let chunk = vec![b'x'; 1 << 20];
    for _ in 0..total / chunk.len() {
        captured.extend(parser.feed(&chunk));
        assert!(
            parser.pending() <= MAX_LINE,
            "{} bytes held",
            parser.pending()
        );
    }
    captured.extend(parser.feed(b"\n"));
    let lines = lines(&captured);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with(&"x".repeat(MAX_LINE)));
    assert!(lines[0].len() < MAX_LINE + 100, "{} bytes", lines[0].len());
    // Resuming continues after the whole line, not after the part kept.
    assert!(matches!(
        captured.last(),
        Some(Captured::Lines { position: LogPosition { page: 1, offset }, .. }) if *offset == total as u64 + 1
    ));
}

#[test]
fn many_lines_in_one_chunk_come_in_batches_each_with_where_to_resume() {
    let line = "y".repeat(999);
    let count = 3000_u64;
    let mut parser = Parser::default();
    let mut body = format!("@@page 1 0 {}\n", count * 1000).into_bytes();
    for _ in 0..count {
        body.extend_from_slice(line.as_bytes());
        body.push(b'\n');
    }
    let captured = parser.feed(&body);
    let mut resumed_at = 0;
    for batch in &captured {
        let Captured::Lines { lines, position } = batch else {
            panic!("{batch:?}");
        };
        assert!(
            lines.len() * 1000 <= MAX_LINE + 1000,
            "{} lines",
            lines.len()
        );
        resumed_at += lines.len() as u64 * 1000;
        assert_eq!(position.offset, resumed_at);
    }
    assert_eq!(resumed_at, count * 1000);
}
