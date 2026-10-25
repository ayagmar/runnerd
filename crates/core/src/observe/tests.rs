//! Metrics from fixture cgroup and proc trees, the capture script's output, and mirror attribution. Never the
//! host's cgroups.

use std::path::Path;

use super::capture::{Captured, MAX_LINE, Parser};
use super::metrics::{Events, Paths, Sampler, Tracked, read_holders, read_host_memory};
use super::mirror::{self, Leases};
use crate::api;
use crate::store::{LOG_BACKLOG, LogPosition, MirrorPosition};
use crate::testdir::TestDir;

/// Writes a container's cgroup files under the `ci.slice` fixture, and its network counters: `current` bytes in
/// `memory.current`, `cache` of them page cache on the file lists (a third active, the rest inactive).
fn container(paths: &Paths, id: &str, pid: u32, current: u64, cache: u64, usage_usec: u64) {
    let cgroup = paths.container("ci.slice", id);
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(cgroup.join("memory.current"), format!("{current}\n")).unwrap();
    let active = cache / 3;
    std::fs::write(
        cgroup.join("memory.stat"),
        format!(
            "anon 100\nfile {cache}\nkernel 400\nshmem 0\ninactive_anon 50\nactive_anon 50\n\
             inactive_file {}\nactive_file {active}\nslab_reclaimable 300\nslab_unreclaimable 100\n",
            cache - active
        ),
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
fn memory_leaves_out_page_cache_the_kernel_can_take_back_and_cpu_is_use_between_samples() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000_000, 1_500_000, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    let (first, _) = state.sample(&runners, 10_000);
    // Two seconds later the container used 3 CPU seconds: 1.5 cores. Its build read the same files twice, so a
    // third of its page cache is active: page cache all the same, so left out.
    container(&paths, "a", 10, 5_000_000, 3_000_000, 4_000_000);
    let (second, _) = state.sample(&runners, 12_000);
    let observed: Vec<(u64, u64)> = first
        .iter()
        .chain(&second)
        .map(|sample| (sample.memory, sample.cpu))
        .collect();
    assert_eq!(observed, [(2_500_000, 0), (2_000_000, 1500)]);
    // eth0 only: not loopback, not the inner Docker Engine's bridge.
    assert_eq!(
        (second[0].network_received, second[0].network_sent),
        (346_864, 129_381)
    );
}

#[test]
fn a_job_whose_page_cache_fills_its_cap_peaks_without_it_and_keeps_its_slab() {
    // As measured on a runner at its 6.25G cap: 0.5G of anonymous memory, tmpfs and kernel memory, the rest page
    // cache. `docker stats` would say 3.6G. Its reclaimable slab stays: dentries and inodes of open files are
    // counted as reclaimable but can't be freed while held.
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    let cgroup = paths.container("ci.slice", "a");
    container(&paths, "a", 10, 0, 0, 0);
    std::fs::write(cgroup.join("memory.current"), "6710885400\n").unwrap();
    std::fs::write(
        cgroup.join("memory.stat"),
        "anon 329342080\nfile 6339871296\nkernel 41672024\nshmem 46301184\ninactive_anon 0\n\
         active_anon 375635072\ninactive_file 3000000000\nactive_file 3293570112\nslab_reclaimable 8112344\n\
         slab_unreclaimable 2606528\n",
    )
    .unwrap();
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let (samples, _) = state.sample(&[tracked("runner-a", "a", 10)], 1_000);
    // anon + shmem + all of its kernel memory.
    assert_eq!(samples[0].memory, 329_342_080 + 46_301_184 + 41_672_024);
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

#[test]
fn running_out_of_memory_and_a_kill_for_it_show_in_a_containers_samples() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 0);
    let events = paths.container("ci.slice", "a").join("memory.events");
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    let mut events_after = |text: &str, at| {
        std::fs::write(&events, text).unwrap();
        state.sample(&runners, at).0[0].events
    };
    // Reclaim at the cap (`max`) is not running out of memory: page cache filling the cap counts there too.
    assert_eq!(
        events_after("low 0\nhigh 0\nmax 10171\noom 0\noom_kill 0\n", 1_000),
        Events {
            oom: Some(0),
            oom_kill: Some(0),
            sock_throttled: None,
        },
        "zero counters say the kernel never ran out of memory"
    );
    assert_eq!(
        events_after("low 0\nhigh 0\nmax 10173\noom 1\noom_kill 1\n", 3_000),
        Events {
            oom: Some(1),
            oom_kill: Some(1),
            sock_throttled: None,
        }
    );
    // Reclaim that went badly held its sockets' receive windows small: its downloads slowed.
    assert_eq!(
        events_after(
            "low 0\nhigh 0\nmax 10173\noom 1\noom_kill 1\nsock_throttled 846\n",
            4_000
        )
        .sock_throttled,
        Some(846),
        "the throttled sockets are counted"
    );
    // A counter the file lacks, or that isn't a number, says nothing either way.
    assert_eq!(
        events_after("low 0\nhigh 0\nmax 10173\n", 5_000),
        Events::default()
    );
    assert_eq!(
        events_after("max 10173\noom many\noom_kill 2\n", 7_000),
        Events {
            oom: None,
            oom_kill: Some(2),
            sock_throttled: None,
        }
    );
    std::fs::remove_file(&events).unwrap();
    assert_eq!(
        state.sample(&runners, 9_000).0[0].events,
        Events::default(),
        "no memory.events"
    );
}

#[test]
fn the_slice_counts_its_active_page_cache_for_the_high_water_mark() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    std::fs::write(slice.join("memory.current"), "10000\n").unwrap();
    std::fs::write(
        slice.join("memory.stat"),
        "anon 4000\nfile 6000\ninactive_file 4000\nactive_file 2000\n",
    )
    .unwrap();
    std::fs::write(slice.join("cpu.stat"), "usage_usec 0\n").unwrap();
    let (_, host) = Sampler::new(paths, "ci.slice").sample(&[], 1_000);
    assert_eq!(host.map(|host| host.memory), Some(6000));
}

#[test]
fn beside_the_hosts_memory_the_slice_counts_no_slab_that_memavailable_counts_already() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    std::fs::create_dir_all(&paths.proc).unwrap();
    std::fs::write(
        slice.join("memory.current"),
        "10000
",
    )
    .unwrap();
    std::fs::write(
        slice.join("memory.stat"),
        "anon 3000
file 6000
inactive_file 4000
active_file 2000
slab_reclaimable 800
",
    )
    .unwrap();
    std::fs::write(
        slice.join("cpu.stat"),
        "usage_usec 0
",
    )
    .unwrap();
    std::fs::write(
        paths.proc.join("meminfo"),
        "MemTotal: 64 kB
MemAvailable: 32 kB
",
    )
    .unwrap();
    let memory = Sampler::new(paths, "ci.slice").host_memory(1_000);
    // 10000 less 6000 of page cache and 800 of reclaimable slab.
    assert_eq!(memory.map(|memory| memory.slice), Some(3200));
}

/// Writes a process under the `proc` fixture: its name, the cgroup it is in, and what it holds in `RssAnon` and
/// `RssShmem`, in MiB.
fn process(paths: &Paths, pid: u32, name: &str, cgroup: &str, anon: u64, shmem: u64) {
    let dir = paths.proc.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
    std::fs::write(
        dir.join("status"),
        format!(
            "Name:\t{name}\nUmask:\t0022\nState:\tS (sleeping)\nVmRSS:\t{} kB\nRssAnon:\t{} kB\n\
             RssFile:\t 9999 kB\nRssShmem:\t{} kB\n",
            (anon + shmem + 10) << 10,
            anon << 10,
            shmem << 10
        ),
    )
    .unwrap();
}

fn holders(list: &[(&str, u64)]) -> Vec<api::Holder> {
    list.iter()
        .map(|(name, mib)| api::Holder {
            name: (*name).to_owned(),
            memory: mib << 20,
        })
        .collect()
}

#[test]
fn the_programs_outside_the_slice_holding_the_most_memory_are_named_largest_first() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    let terminal = "/user.slice/user-1000.slice/user@1000.service/app.slice/terminal.scope";
    // Two Gradle daemons and an emulator in the owner's terminal: one name per program, its processes summed.
    process(&paths, 100, "java", terminal, 5 * 1024, 0);
    process(&paths, 101, "java", terminal, 4 * 1024, 300);
    process(&paths, 200, "qemu-system-x86", terminal, 1024, 5 * 1024);
    process(&paths, 300, "firefox", "/user.slice/firefox.scope", 2048, 0);
    process(
        &paths,
        400,
        "postgres",
        "/system.slice/postgresql.service",
        1024,
        0,
    );
    // Runners and their jobs in the slice, however deep, are not outside it.
    process(
        &paths,
        500,
        "java",
        "/ci.slice/docker-abc.scope/init.scope",
        20 * 1024,
        0,
    );
    process(&paths, 501, "dockerd", "/ci.slice", 30 * 1024, 0);
    // A slice whose name only starts like it is another.
    process(&paths, 600, "node", "/ci.slice-other/x.scope", 700, 0);
    // Too small to name, alone or together.
    process(&paths, 700, "bash", terminal, 300, 0);
    process(&paths, 701, "bash", terminal, 200, 0);
    // A kernel thread holds nothing of its own.
    std::fs::create_dir_all(paths.proc.join("2")).unwrap();
    std::fs::write(paths.proc.join("2/cgroup"), "0::/\n").unwrap();
    std::fs::write(paths.proc.join("2/status"), "Name:\tkthreadd\n").unwrap();
    assert_eq!(
        read_holders(&paths, "ci.slice"),
        holders(&[
            ("java", 9 * 1024 + 300),
            ("qemu-system-x86", 6 * 1024),
            ("firefox", 2048)
        ]),
        "the three largest, the slice's left out"
    );
}

#[test]
fn the_hosts_processes_are_read_for_what_holds_its_memory_only_every_holders_interval() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    std::fs::create_dir_all(&paths.proc).unwrap();
    std::fs::write(
        paths.proc.join("meminfo"),
        "MemTotal: 67108864 kB\nMemAvailable: 2097152 kB\n",
    )
    .unwrap();
    process(&paths, 100, "java", "/user.slice/a.scope", 9 * 1024, 0);
    let mut sampler = Sampler::new(paths.clone(), "ci.slice");
    let read = |sampler: &mut Sampler, at| sampler.host_memory(at).unwrap().holders;
    assert_eq!(read(&mut sampler, 1_000), holders(&[("java", 9 * 1024)]));
    process(
        &paths,
        200,
        "qemu-system-x86",
        "/user.slice/b.scope",
        6 * 1024,
        0,
    );
    assert_eq!(
        read(&mut sampler, 29_000),
        holders(&[("java", 9 * 1024)]),
        "the last reading, under 30 s old"
    );
    assert_eq!(
        read(&mut sampler, 31_000),
        holders(&[("java", 9 * 1024), ("qemu-system-x86", 6 * 1024)]),
        "read again after 30 s"
    );
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

#[test]
fn the_hosts_memory_and_how_long_its_tasks_stalled_for_it_are_read_from_proc() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    std::fs::create_dir_all(paths.proc.join("pressure")).unwrap();
    std::fs::write(
        paths.proc.join("meminfo"),
        "MemTotal:       65184256 kB\nMemFree:         1048576 kB\nMemAvailable:   27108076 kB\n",
    )
    .unwrap();
    assert_eq!(
        read_host_memory(&paths, 5, 7).map(|memory| (memory.full, memory.some)),
        Some((None, None)),
        "a kernel without pressure stall information still says what is available"
    );
    std::fs::write(
        paths.proc.join("pressure/memory"),
        "some avg10=12.34 avg60=0.50 avg300=0.00 total=1626686\n\
         full avg10=6.70 avg60=1.05 avg300=0.00 total=1535492\n",
    )
    .unwrap();
    assert_eq!(
        read_host_memory(&paths, 5 << 30, 7),
        Some(api::HostMemory {
            at: 7,
            total: 65_184_256 * 1024,
            available: 27_108_076 * 1024,
            slice: 5 << 30,
            some: Some(api::Pressure {
                avg10: 1234,
                avg60: 50
            }),
            full: Some(api::Pressure {
                avg10: 670,
                avg60: 105
            }),
            holders: Vec::new(),
        })
    );
    std::fs::write(paths.proc.join("meminfo"), "MemTotal: 1 kB\n").unwrap();
    assert_eq!(read_host_memory(&paths, 0, 7), None, "no MemAvailable");
}
