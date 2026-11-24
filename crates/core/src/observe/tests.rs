//! Metrics from fixture cgroup and proc trees, the capture script's output, and mirror attribution. Never the
//! host's cgroups.

use std::path::Path;

use super::capture::{Captured, MAX_LINE, Parser, worker_job, worker_run};
use super::disk::{self, Disks, Window, Windows, Written};
use super::metrics::{CpuHeld, Events, Paths, Sampler, Tracked, read_holders, read_host_memory};
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
        sys: dir.join("sys"),
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
    let first = state.sample(&runners, 10_000).samples;
    // Two seconds later the container used 3 CPU seconds: 1.5 cores. Its build read the same files twice, so a
    // third of its page cache is active: page cache all the same, so left out.
    container(&paths, "a", 10, 5_000_000, 3_000_000, 4_000_000);
    let second = state.sample(&runners, 12_000).samples;
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
    let samples = state.sample(&[tracked("runner-a", "a", 10)], 1_000).samples;
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
    assert_eq!(state.sample(&runners, 1_000).samples.len(), 2);
    std::fs::remove_dir_all(paths.container("ci.slice", "a")).unwrap();
    let after = state.sample(&runners, 3_000);
    let names: Vec<&str> = after
        .samples
        .iter()
        .map(|sample| sample.runner.as_str())
        .collect();
    assert_eq!(names, ["runner-b"]);
    assert_eq!(after.gone, ["runner-a"], "its cgroup went after a read");
    assert!(
        state.sample(&runners, 5_000).gone.is_empty(),
        "a gone container is told once"
    );
}

#[test]
fn only_a_missing_cgroup_reads_as_gone() {
    // A read that fails while the cgroup stands says nothing of the container: it must keep counting.
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 0);
    let cgroup = paths.container("ci.slice", "a");
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    state.sample(&runners, 1_000);

    std::fs::write(cgroup.join("memory.current"), "not a number\n").unwrap();
    let unreadable = state.sample(&runners, 3_000);
    assert!(
        unreadable.gone.is_empty(),
        "a bad file is not a gone container"
    );
    assert!(unreadable.samples.is_empty(), "nor a reading");

    container(&paths, "a", 10, 4_000, 0, 0);
    std::fs::remove_file(cgroup.join("memory.stat")).unwrap();
    assert!(
        state.sample(&runners, 5_000).gone.is_empty(),
        "one missing file in a cgroup that stands is not a gone container"
    );

    std::fs::remove_dir_all(&cgroup).unwrap();
    assert_eq!(
        state.sample(&runners, 7_000).gone,
        ["runner-a"],
        "failed reads keep what the next one needs to tell it went"
    );
}

#[test]
fn a_container_that_comes_back_is_read_though_its_old_process_gives_no_sample() {
    // Docker restarts a container into the same cgroup with a new process, so the read alone has to say it is back.
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 0);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    state.sample(&runners, 1_000);
    std::fs::remove_dir_all(paths.container("ci.slice", "a")).unwrap();
    assert_eq!(state.sample(&runners, 3_000).gone, ["runner-a"]);

    container(&paths, "a", 10, 4_000, 0, 0);
    std::fs::remove_dir_all(paths.proc.join("10")).unwrap();
    let back = state.sample(&runners, 5_000);
    assert_eq!(back.read, ["runner-a"], "the cgroup is back");
    assert!(back.samples.is_empty(), "its old process is not");
}

#[test]
fn a_container_never_read_is_not_called_gone() {
    // A slice whose containers live elsewhere, such as under another cgroup driver: live runners with no files here.
    let dir = TestDir::new("observe");
    let mut state = Sampler::new(paths(&dir.0), "ci.slice");
    let sampled = state.sample(&[tracked("runner-a", "a", 10)], 1_000);
    assert!(sampled.gone.is_empty(), "it was never there to go");
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
        state.sample(&runners, at).samples[0].events
    };
    // Reclaim at the cap (`max`) is not running out of memory: page cache filling the cap counts there too.
    assert_eq!(
        events_after("low 0\nhigh 0\nmax 10171\noom 0\noom_kill 0\n", 1_000),
        Events {
            oom: Some(0),
            local_oom: None,
            oom_kill: Some(0),
            sock_throttled: None,
        },
        "zero counters say the kernel never ran out of memory"
    );
    assert_eq!(
        events_after("low 0\nhigh 0\nmax 10173\noom 1\noom_kill 1\n", 3_000),
        Events {
            oom: Some(1),
            local_oom: None,
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
            local_oom: None,
            oom_kill: Some(2),
            sock_throttled: None,
        }
    );
    std::fs::remove_file(&events).unwrap();
    assert_eq!(
        state.sample(&runners, 9_000).samples[0].events,
        Events::default(),
        "no memory.events"
    );
}

#[test]
fn a_limit_a_jobs_own_container_hit_is_told_from_the_runners_own() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 0);
    let cgroup = paths.container("ci.slice", "a");
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    // A service container the job started with its own 512M limit ran out of memory inside the runner's cgroup.
    std::fs::write(cgroup.join("memory.events"), "max 3\noom 1\noom_kill 1\n").unwrap();
    std::fs::write(
        cgroup.join("memory.events.local"),
        "max 0\noom 0\noom_kill 0\n",
    )
    .unwrap();
    let inner = state.sample(&runners, 1_000).samples[0].events;
    assert_eq!((inner.oom, inner.local_oom), (Some(1), Some(0)));
    // Then the runner's own cap stopped an allocation.
    std::fs::write(
        cgroup.join("memory.events.local"),
        "max 9\noom 1\noom_kill 0\n",
    )
    .unwrap();
    assert_eq!(
        state.sample(&runners, 3_000).samples[0].events.local_oom,
        Some(1)
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
    let host = Sampler::new(paths, "ci.slice").sample(&[], 1_000).host;
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
    process_bytes(
        paths,
        pid,
        name.as_bytes(),
        &format!("0::{cgroup}\n"),
        anon,
        shmem,
    );
}

/// `process` with a name of any bytes and the whole of `/proc/<pid>/cgroup`, beside cgroup v2's root.
fn process_bytes(paths: &Paths, pid: u32, name: &[u8], cgroup: &str, anon: u64, shmem: u64) {
    std::fs::create_dir_all(&paths.cgroup).unwrap();
    std::fs::write(paths.cgroup.join("cgroup.controllers"), "cpu memory\n").unwrap();
    let dir = paths.proc.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cgroup"), cgroup).unwrap();
    let mut status = b"Name:\t".to_vec();
    status.extend_from_slice(name);
    status.extend_from_slice(
        format!(
            "\nUmask:\t0022\nState:\tS (sleeping)\nVmRSS:\t{} kB\nRssAnon:\t{} kB\n\
             RssFile:\t 9999 kB\nRssShmem:\t{} kB\n",
            (anon + shmem + 10) << 10,
            anon << 10,
            shmem << 10
        )
        .as_bytes(),
    );
    std::fs::write(dir.join("status"), status).unwrap();
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
fn where_cgroup_v2_cannot_place_a_process_no_runner_passes_for_a_program_outside_ci() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    // A hybrid host: the runner's memory cgroup is v1's, and v2 says only the root.
    process_bytes(
        &paths,
        500,
        b"java",
        "5:memory:/ci.slice/docker-a.scope\n0::/\n",
        20 * 1024,
        0,
    );
    // A process with only v1 memberships.
    process_bytes(
        &paths,
        501,
        b"dockerd",
        "5:memory:/ci.slice/docker-b.scope\n",
        10 * 1024,
        0,
    );
    std::fs::remove_file(paths.cgroup.join("cgroup.controllers")).unwrap();
    assert_eq!(
        read_holders(&paths, "ci.slice"),
        [],
        "no v2 at the cgroup root"
    );
    // On cgroup v2, a process without a v2 membership still can't be told apart from a runner.
    std::fs::write(paths.cgroup.join("cgroup.controllers"), "memory\n").unwrap();
    std::fs::write(
        paths.proc.join("500/cgroup"),
        "0::/ci.slice/docker-a.scope\n",
    )
    .unwrap();
    process(
        &paths,
        600,
        "qemu-system-x86",
        "/user.slice/a.scope",
        6 * 1024,
        0,
    );
    assert_eq!(
        read_holders(&paths, "ci.slice"),
        holders(&[("qemu-system-x86", 6 * 1024)]),
        "only the program v2 puts outside the slice"
    );
}

#[test]
fn a_name_cut_inside_a_character_keeps_its_programs_memory() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    // A 16-byte name ending in "é" that the kernel cut at 15 bytes, inside the character.
    let mut name = b"gradle-daemon-".to_vec();
    name.push(0xc3);
    process_bytes(&paths, 100, &name, "0::/user.slice/a.scope\n", 2048, 0);
    assert_eq!(
        read_holders(&paths, "ci.slice"),
        holders(&[("gradle-daemon-\u{fffd}", 2048)])
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
fn a_stopped_runners_worker_log_names_its_job_by_the_check_run_id_alone() {
    // The job message's `github` context, as the runner logs it: other numbers come before the job's.
    let log = r#"[2026-10-31 12:00:01Z INFO Worker] Job message:
 {
  "contextData": {
    "github": {
      "t": 2,
      "d": [
        {
          "k": "run_id",
          "v": "39840158500"
        },
        {
          "k": "run_number",
          "v": 17.0
        },
        {
          "k": "check_run_id",
          "v": 120038758684.0
        },
"#;
    assert_eq!(worker_job(log), Some(120_038_758_684));
    assert_eq!(worker_run(log), Some(39_840_158_500), "quoted in the log");
    let before =
        "[2026-10-31 12:00:00Z INFO Worker] Waiting to receive the job message from the channel.\n";
    assert_eq!(worker_job(before), None, "no job taken yet");
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
            ceiling: None,
            swap: None,
        })
    );
    std::fs::write(paths.proc.join("meminfo"), "MemTotal: 1 kB\n").unwrap();
    assert_eq!(read_host_memory(&paths, 0, 7), None, "no MemAvailable");
}

#[test]
fn swap_is_read_from_the_same_meminfo_and_absent_without_a_swap_device() {
    // Catches a host without swap reported as one with 0B of it, and a half-read figure reported as a whole one.
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    std::fs::create_dir_all(&paths.proc).unwrap();
    let swap = |meminfo: &str| {
        std::fs::write(
            paths.proc.join("meminfo"),
            format!("MemTotal: 65184256 kB\nMemAvailable: 27108076 kB\n{meminfo}"),
        )
        .unwrap();
        read_host_memory(&paths, 0, 7).unwrap().swap
    };
    assert_eq!(
        swap("SwapTotal: 16252924 kB\nSwapFree: 3544612 kB\n"),
        Some(api::Swap {
            total: 16_252_924 * 1024,
            free: 3_544_612 * 1024,
        })
    );
    assert_eq!(swap("SwapTotal: 0 kB\nSwapFree: 0 kB\n"), None, "no device");
    assert_eq!(swap(""), None, "no swap lines");
    assert_eq!(swap("SwapTotal: 1024 kB\n"), None, "no SwapFree");
}

#[test]
fn what_held_a_containers_cpu_back_is_read_where_the_kernel_keeps_it_and_unknown_where_not() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    // Without the cpu controller `cpu.stat` has no periods, and without pressure stall information there is no
    // `cpu.pressure`: the sample is still taken, its CPU figures unknown.
    container(&paths, "a", 10, 4_000, 0, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    let bare = state.sample(&runners, 1_000).samples;
    assert_eq!(bare.len(), 1, "the sample is taken");
    assert_eq!(bare[0].cpu_held, CpuHeld::default());
    let cgroup = paths.container("ci.slice", "a");
    std::fs::write(
        cgroup.join("cpu.stat"),
        "usage_usec 3000000\nuser_usec 1\nsystem_usec 1\nnr_periods 1040\nnr_throttled 205\n\
         throttled_usec 9000000\nnr_bursts 0\nburst_usec 0\n",
    )
    .unwrap();
    std::fs::write(
        cgroup.join("cpu.pressure"),
        "some avg10=12.50 avg60=8.00 avg300=2.00 total=409000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=1000\n",
    )
    .unwrap();
    let held = state.sample(&runners, 3_000).samples;
    assert_eq!(
        held[0].cpu_held,
        CpuHeld {
            periods: Some(1040),
            throttled: Some(205),
            stalled: Some(409_000),
        }
    );
    assert_eq!(held[0].cpu, 1000, "CPU use reads as before");
}

/// A host of fixture files whose Docker root, `/var/lib/docker`, is on partition `nvme9n1p2` (259:7) of disk
/// `nvme9n1` (259:5), on `fs`: btrfs, with UUID `u-1`, or ext4.
fn disk_host(paths: &Paths, fs: &str) {
    let devices = paths.sys.join("devices/pci0/nvme/nvme9n1");
    std::fs::create_dir_all(devices.join("nvme9n1p2")).unwrap();
    std::fs::write(devices.join("dev"), "259:5\n").unwrap();
    std::fs::write(devices.join("nvme9n1p2/dev"), "259:7\n").unwrap();
    std::fs::write(devices.join("nvme9n1p2/partition"), "2\n").unwrap();
    let class = paths.sys.join("class/block");
    std::fs::create_dir_all(&class).unwrap();
    std::os::unix::fs::symlink(devices.join("nvme9n1p2"), class.join("nvme9n1p2")).unwrap();
    std::os::unix::fs::symlink(&devices, class.join("nvme9n1")).unwrap();
    std::fs::create_dir_all(paths.sys.join("dev/block")).unwrap();
    std::os::unix::fs::symlink(devices.join("nvme9n1p2"), paths.sys.join("dev/block/259:7"))
        .unwrap();
    let mount_device = if fs == "btrfs" {
        let btrfs = paths.sys.join("fs/btrfs/u-1");
        std::fs::create_dir_all(btrfs.join("devices")).unwrap();
        std::os::unix::fs::symlink(devices.join("nvme9n1p2"), btrfs.join("devices/nvme9n1p2"))
            .unwrap();
        "0:33"
    } else {
        "259:7"
    };
    std::fs::create_dir_all(paths.proc.join("self")).unwrap();
    std::fs::write(
        paths.proc.join("self/mountinfo"),
        format!(
            "22 1 0:21 / /proc rw,nosuid - proc proc rw\n\
             40 1 {mount_device} /@ / rw,noatime shared:1 - {fs} /dev/nvme9n1p2 rw\n\
             41 40 0:44 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw\n\
             45 1 0:29 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"
        ),
    )
    .unwrap();
    drive(paths, 1000, 10);
}

/// Sets the drive's sectors written and btrfs's commits so far.
fn drive(paths: &Paths, sectors: u64, commits: u64) {
    std::fs::write(
        paths.sys.join("devices/pci0/nvme/nvme9n1/stat"),
        format!("100 0 2000 50 300 0 {sectors} 900 0 400 950 0 0 0 0 0 0\n"),
    )
    .unwrap();
    let btrfs = paths.sys.join("fs/btrfs/u-1");
    if btrfs.exists() {
        std::fs::write(
            btrfs.join("commit_stats"),
            format!("commits {commits}\ncur_commit_ms 0\nlast_commit_ms 15\n"),
        )
        .unwrap();
    }
}

/// Writes a cgroup's `io.stat`: zram's swap writes, which never reach the drive, beside `bytes` to the drive.
fn io_stat(cgroup: &Path, bytes: u64) {
    std::fs::write(
        cgroup.join("io.stat"),
        format!(
            "253:0 rbytes=0 wbytes=999999 rios=0 wios=99 dbytes=0 dios=0\n\
             259:5 rbytes=4096 wbytes={bytes} rios=1 wios={} dbytes=0 dios=0\n",
            bytes / 4096
        ),
    )
    .unwrap();
}

#[test]
fn docker_s_root_on_a_btrfs_partition_reads_as_its_whole_disk_and_its_file_system() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    assert_eq!(disks.name(), "nvme9n1");
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    io_stat(&slice, 8192);
    let reading = disks.read(&paths, &slice, 1_000);
    let mut windows = Windows::default();
    assert_eq!(windows.add(reading), None);
    drive(&paths, 3000, 25);
    io_stat(&slice, 8192 + 4096);
    let closed = windows
        .add(disks.read(&paths, &slice, 1_000 + 600_000))
        .unwrap();
    assert_eq!(
        closed,
        Window {
            start: 1_000,
            end: 601_000,
            device: "nvme9n1".to_owned(),
            drive: Some(2000 * 512),
            slice: Some(4096),
            daemon: None,
            owners: None,
            commits: Some(15),
            store: None,
        }
    );
}

#[test]
fn a_file_system_other_than_btrfs_has_no_commits_and_an_unreadable_drive_no_figure() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "ext4");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker/volumes")).unwrap();
    assert_eq!(disks.name(), "nvme9n1");
    // No io controller for the slice: no io.stat.
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    drive(&paths, 1500, 0);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    assert_eq!(window.drive, Some(500 * 512));
    assert_eq!(window.slice, None, "absent, not 0");
    assert_eq!(window.commits, None, "absent, not 0");
    std::fs::remove_file(paths.sys.join("devices/pci0/nvme/nvme9n1/stat")).unwrap();
    windows.add(disks.read(&paths, &slice, 900_000));
    assert_eq!(
        windows.close().unwrap().drive,
        None,
        "a window with an unread drive has no figure"
    );
}

#[test]
fn a_host_without_mountinfo_or_sysfs_names_no_disks() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    assert_eq!(Disks::resolve(&paths, Path::new("/var/lib/docker")), None);
    disk_host(&paths, "btrfs");
    std::fs::remove_dir_all(paths.sys.join("class")).unwrap();
    assert_eq!(Disks::resolve(&paths, Path::new("/var/lib/docker")), None);
}

#[test]
fn a_container_s_writes_count_only_the_drive_under_docker_s_root() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    container(&paths, "a", 10, 4_000_000, 1_500_000, 1_000_000);
    let cgroup = paths.container("ci.slice", "a");
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    assert_eq!(
        state.sample(&runners, 1_000).samples[0].written,
        None,
        "unknown until the disks are"
    );
    state.watch_disk(disks.clone());
    assert_eq!(
        state.sample(&runners, 3_000).samples[0].written,
        None,
        "no io.stat: no io controller"
    );
    std::fs::write(
        cgroup.join("io.stat"),
        "253:0 rbytes=0 wbytes=4096 rios=0 wios=1 dbytes=0 dios=0\n",
    )
    .unwrap();
    assert_eq!(
        state.sample(&runners, 5_000).samples[0].written,
        Some(Written { bytes: 0, ios: 0 }),
        "no line for the drive: none written to it yet"
    );
    io_stat(&cgroup, 40_960);
    let sample = &state.sample(&runners, 7_000).samples[0];
    assert_eq!(
        sample.written,
        Some(Written {
            bytes: 40_960,
            ios: 10
        })
    );
}

#[test]
fn a_container_the_engine_let_go_of_whose_cgroup_went_ended_within_a_sample_of_its_last_read() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000_000, 1_500_000, 1_000_000);
    container(&paths, "b", 11, 4_000_000, 1_500_000, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let both = [tracked("runner-a", "a", 10), tracked("runner-b", "b", 11)];
    state.sample(&both, 1_000);
    // The engine saw both stop and let go of a; b's cgroup goes while it is still tracked.
    std::fs::remove_dir_all(paths.container("ci.slice", "a")).unwrap();
    std::fs::remove_dir_all(paths.container("ci.slice", "b")).unwrap();
    let sampled = state.sample(&both[1..], 3_000);
    let mut ended = sampled.ended.clone();
    ended.sort();
    assert_eq!(ended, ["runner-a", "runner-b"]);
    assert_eq!(
        sampled.gone,
        ["runner-b"],
        "admission hears only of the tracked one"
    );
    assert!(
        state.sample(&[], 5_000).ended.is_empty(),
        "an end is told once"
    );
}

#[test]
fn a_container_let_go_of_whose_cgroup_stays_has_not_ended() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000_000, 1_500_000, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    state.sample(&[tracked("runner-a", "a", 10)], 1_000);
    assert!(
        state.sample(&[], 3_000).ended.is_empty(),
        "its cgroup is still there"
    );
}

#[test]
fn a_restart_starts_a_new_window_and_a_counter_that_fell_adds_nothing() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    io_stat(&slice, 0);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    drive(&paths, 2000, 20);
    windows.add(disks.read(&paths, &slice, 100_000));
    // The disk's counters fell, as after it was taken out and put back.
    drive(&paths, 500, 21);
    windows.add(disks.read(&paths, &slice, 200_000));
    let stopped = windows.close().unwrap();
    assert_eq!((stopped.start, stopped.end), (0, 200_000));
    assert_eq!(stopped.drive, Some(1000 * 512));
    assert_eq!(windows.close(), None, "a stop stores its window once");
    // The next daemon's first reading only starts a window: the time no daemon ran is in none.
    let mut next = Windows::default();
    drive(&paths, 9000, 30);
    assert_eq!(next.add(disks.read(&paths, &slice, 900_000)), None);
    assert_eq!(next.close(), None);
}

#[tokio::test]
async fn the_sampler_stores_the_drive_s_open_window_as_soon_as_the_engine_lets_go_of_it() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    io_stat(&slice, 0);
    let io = paths.proc.join("self/io");
    std::fs::write(&io, "rchar: 1\nwchar: 1\nwrite_bytes: 4096\n").unwrap();
    let store = crate::store::Store::open(&dir.0.join("runnerd.db")).unwrap();
    let (tracked, watched) = tokio::sync::watch::channel(Vec::new());
    let (disks, disks_read) = tokio::sync::oneshot::channel();
    disks
        .send(Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap())
        .unwrap();
    let sampler = tokio::spawn(super::metrics::run(
        Sampler::new(paths.clone(), "ci.slice"),
        watched,
        store.clone(),
        disks_read,
    ));
    // Two passes read the drive: the first starts the window, the second adds to it.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drive(&paths, 3000, 12);
    std::fs::write(&io, "rchar: 1\nwchar: 1\nwrite_bytes: 12288\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    // A daemon that stops closes its history right after the engine returns: the window must be sent by then.
    drop(tracked);
    tokio::time::timeout(std::time::Duration::from_millis(500), sampler)
        .await
        .expect("the sampler ends when the engine lets go of it")
        .unwrap();
    store.flush().await;
    let windows = store.insights(0).await.unwrap().disk;
    assert_eq!(windows.len(), 1, "the open window is stored: {windows:?}");
    assert_eq!(windows[0].drive, Some(2000 * 512));
    assert_eq!(windows[0].daemon, Some(8192), "the daemon's own writes");
}

#[test]
fn docker_s_root_in_a_file_counts_the_loop_device_s_line_and_reads_the_drive_under_its_backing_file()
 {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    // An ext4 file system in /var/lib/runnerd/docker.img on the btrfs root, on loop0, mounted at Docker's root.
    let lo = paths.sys.join("devices/virtual/block/loop0");
    std::fs::create_dir_all(lo.join("loop")).unwrap();
    std::fs::write(lo.join("dev"), "7:0\n").unwrap();
    std::fs::write(
        lo.join("loop/backing_file"),
        "/var/lib/runnerd/docker.img\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join("class/block/loop0")).unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join("dev/block/7:0")).unwrap();
    let mountinfo = paths.proc.join("self/mountinfo");
    let mut mounts = std::fs::read_to_string(&mountinfo).unwrap();
    mounts.push_str("50 40 7:0 / /var/lib/docker rw,relatime shared:9 - ext4 /dev/loop0 rw\n");
    std::fs::write(&mountinfo, mounts).unwrap();
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    assert_eq!(disks.name(), "nvme9n1", "the drive is the backing file's");
    assert_eq!(disks.store().as_deref(), Some("loop0"));
    // The container's line for the loop device is what it wrote to the store; the drive's line, charged to it only
    // some of the time, is not counted beside it.
    let cgroup = dir.0.join("cgroup-a");
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(
        cgroup.join("io.stat"),
        "7:0 rbytes=0 wbytes=8192 rios=0 wios=2 dbytes=0 dios=0\n\
         259:5 rbytes=0 wbytes=4096 rios=0 wios=1 dbytes=0 dios=0\n",
    )
    .unwrap();
    assert_eq!(
        disks.written(&cgroup),
        Some(Written {
            bytes: 8192,
            ios: 2
        })
    );
    // The drive and the commits are the physical disk's and its btrfs's.
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    drive(&paths, 1400, 13);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    assert_eq!(window.drive, Some(400 * 512));
    assert_eq!(window.commits, Some(3));
    assert_eq!(window.store.as_deref(), Some("loop0"));
    // A backing file deleted while attached keeps its path, marked.
    std::fs::write(
        lo.join("loop/backing_file"),
        "/var/lib/runnerd/docker.img (deleted)\n",
    )
    .unwrap();
    assert_eq!(
        Disks::resolve(&paths, Path::new("/var/lib/docker")),
        Some(disks)
    );
}

/// Adds loop device `n` with its backing file at `backing`, an ext4 on it mounted at `point`.
fn loop_device(paths: &Paths, n: u32, backing: &str, point: &str) {
    let lo = paths.sys.join(format!("devices/virtual/block/loop{n}"));
    std::fs::create_dir_all(lo.join("loop")).unwrap();
    std::fs::write(lo.join("dev"), format!("7:{n}\n")).unwrap();
    std::fs::write(lo.join("loop/backing_file"), format!("{backing}\n")).unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join(format!("class/block/loop{n}"))).unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join(format!("dev/block/7:{n}"))).unwrap();
    let mountinfo = paths.proc.join("self/mountinfo");
    let mut mounts = std::fs::read_to_string(&mountinfo).unwrap();
    mounts += &[
        (60 + n).to_string(),
        format!("40 7:{n} / {point} rw,relatime - ext4 /dev/loop{n} rw\n"),
    ]
    .join(" ");
    std::fs::write(&mountinfo, mounts).unwrap();
}

/// `statfs`'s free space by mount point, as the test sets it; any other path can't be measured.
fn statfs(free: &[(&str, u64)]) -> impl Fn(&Path) -> std::io::Result<u64> {
    let free: Vec<(std::path::PathBuf, u64)> = free
        .iter()
        .map(|(path, bytes)| (std::path::PathBuf::from(path), *bytes))
        .collect();
    move |path: &Path| {
        free.iter()
            .find(|(point, _)| point == path)
            .map(|(_, bytes)| *bytes)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }
}

const GIB: u64 = 1 << 30;

#[test]
fn docker_s_free_disk_on_a_plain_root_is_its_own_file_system_s() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let root = Path::new("/var/lib/docker");
    let free = statfs(&[("/var/lib/docker", 100 * GIB), ("/", 30 * GIB)]);
    assert_eq!(disk::free_space(&paths, root, &free).unwrap(), 100 * GIB);
    assert!(
        disk::free_space(&paths, root, statfs(&[])).is_err(),
        "a root that can't be measured is an error, as before"
    );
}

#[test]
fn docker_s_free_disk_in_a_file_is_the_least_of_its_own_and_the_file_system_under_the_file() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    loop_device(&paths, 0, "/var/lib/runnerd/docker.img", "/var/lib/docker");
    let root = Path::new("/var/lib/docker");
    // A 560G sparse file with 100G free inside, on a btrfs with room for more: the ext4's own figure.
    let roomy = statfs(&[("/var/lib/docker", 100 * GIB), ("/", 300 * GIB)]);
    assert_eq!(disk::free_space(&paths, root, &roomy).unwrap(), 100 * GIB);
    // The btrfs under it has less: the file can grow only that far.
    let tight = statfs(&[("/var/lib/docker", 100 * GIB), ("/", 40 * GIB)]);
    assert_eq!(disk::free_space(&paths, root, &tight).unwrap(), 40 * GIB);
    // The file under the mount point can't be searched, the mount point can: the figure is the mount point's.
    // Deleted while attached, the file keeps its blocks on the same file system.
    std::fs::write(
        paths
            .sys
            .join("devices/virtual/block/loop0/loop/backing_file"),
        "/var/lib/runnerd/docker.img (deleted)\n",
    )
    .unwrap();
    assert_eq!(disk::free_space(&paths, root, &tight).unwrap(), 40 * GIB);
    // The side under the file can't be measured: the root's own figure, never 0.
    let unread = statfs(&[("/var/lib/docker", 100 * GIB)]);
    assert_eq!(disk::free_space(&paths, root, &unread).unwrap(), 100 * GIB);
    std::fs::remove_file(
        paths
            .sys
            .join("devices/virtual/block/loop0/loop/backing_file"),
    )
    .unwrap();
    assert_eq!(
        disk::free_space(&paths, root, &tight).unwrap(),
        100 * GIB,
        "a loop device whose backing file can't be read: the root's own"
    );
    std::fs::remove_file(paths.proc.join("self/mountinfo")).unwrap();
    assert_eq!(disk::free_space(&paths, root, &tight).unwrap(), 100 * GIB);
}

#[test]
fn docker_s_free_disk_in_a_file_in_a_file_is_the_least_down_to_the_disk() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    loop_device(&paths, 1, "/data/outer.img", "/srv/outer");
    loop_device(&paths, 0, "/srv/outer/docker.img", "/var/lib/docker");
    let root = Path::new("/var/lib/docker");
    let free = statfs(&[
        ("/var/lib/docker", 100 * GIB),
        ("/srv/outer", 60 * GIB),
        ("/", 30 * GIB),
    ]);
    assert_eq!(disk::free_space(&paths, root, &free).unwrap(), 30 * GIB);
    let middle = statfs(&[("/var/lib/docker", 100 * GIB), ("/srv/outer", 60 * GIB)]);
    assert_eq!(
        disk::free_space(&paths, root, &middle).unwrap(),
        60 * GIB,
        "the disk at the bottom unmeasured: the least of those measured"
    );
}

/// Writes a cgroup's `io.stat` with `bytes` in `ios` writes to the drive, beside zram's.
fn io_stat_of(cgroup: &Path, bytes: u64, ios: u64) {
    std::fs::create_dir_all(cgroup).unwrap();
    std::fs::write(
        cgroup.join("io.stat"),
        format!(
            "253:0 rbytes=0 wbytes=999999 rios=0 wios=99 dbytes=0 dios=0\n\
             259:5 rbytes=4096 wbytes={bytes} rios=1 wios={ios} dbytes=0 dios=0\n"
        ),
    )
    .unwrap();
}

/// A host's cgroups as `io.stat` counts them, in KiB, `step` times over: the runner slice, the host's Docker daemon
/// and containerd in `system.slice` beside other services, `init.scope`, runnerd's own service in `user.slice` beside
/// other programs, and the root's line, which counts every write, `kernel` KiB of them no cgroup's in 16 KiB writes.
fn cgroups(paths: &Paths, step: u64, kernel: u64) {
    const K: u64 = 1024;
    let root = &paths.cgroup;
    let runnerd = "user.slice/user-1000.slice/user@1000.service/app.slice/runnerd.service";
    let at = |path: &str, kib: u64| io_stat_of(&root.join(path), kib * K * step, kib * step);
    at("ci.slice", 100);
    at("system.slice/docker.service", 120);
    at("system.slice/containerd.service", 30);
    at("system.slice", 300);
    at("init.scope", 10);
    at(runnerd, 20);
    at("user.slice", 80);
    // The children's 490 KiB in 490 writes, and the kernel's in 16 KiB writes.
    io_stat_of(root, (490 + kernel) * K * step, (490 + kernel / 16) * step);
    std::fs::write(paths.proc.join("self/cgroup"), format!("0::/{runnerd}\n")).unwrap();
}

/// The drive's 68% "no owner runnerd knows" was the host's Docker daemon, other services and the kernel: each is its
/// own figure now, the runner slice and runnerd taken off the cgroup they sit in so no byte counts twice.
#[test]
fn the_drive_s_writers_split_by_cgroup_and_add_up_to_the_root_s_count() {
    const K: u64 = 1024;
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    cgroups(&paths, 1, 1600);
    drive(&paths, 0, 0);
    let mut windows = Windows::default();
    assert_eq!(windows.add(disks.read(&paths, &slice, 0)), None);
    cgroups(&paths, 3, 1600);
    drive(&paths, 2 * (490 + 1600) * 2, 4);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    let owners = window.owners.expect("split by cgroup");
    assert_eq!(window.slice, Some(200 * K));
    assert_eq!(
        owners,
        api::DriveOwners {
            runnerd: Some(40 * K),
            docker: Some(240 * K),
            containerd: Some(60 * K),
            // system.slice's 600 less Docker's and containerd's, and init.scope's 20.
            system: (600 - 240 - 60 + 20) * K,
            // user.slice's 160 less runnerd's.
            user: 120 * K,
            kernel: 3200 * K,
            kernel_ios: 200,
        }
    );
    assert_eq!(owners.kernel_write(), Some(16 * K));
    let named = window.slice.unwrap()
        + owners.runnerd.unwrap()
        + owners.docker.unwrap()
        + owners.containerd.unwrap()
        + owners.system
        + owners.user
        + owners.kernel;
    assert_eq!(
        named,
        2 * (490 + 1600) * K,
        "every byte the root counted, once"
    );

    // A host without the host's Docker daemon's cgroup: its share is absent, not 0, and its bytes are no one's.
    cgroups_without_docker(&paths, 4);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    cgroups_without_docker(&paths, 5);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    let owners = window.owners.unwrap();
    assert_eq!(owners.docker, None);
    assert_eq!(owners.containerd, None);
    assert_eq!(
        owners.system,
        310 * K,
        "system.slice's 300 and init.scope's 10"
    );
}

/// [`cgroups`] at `step` with the host's Docker daemon and containerd gone from `system.slice`.
fn cgroups_without_docker(paths: &Paths, step: u64) {
    cgroups(paths, step, 1600);
    std::fs::remove_dir_all(paths.cgroup.join("system.slice/docker.service")).unwrap();
    std::fs::remove_dir_all(paths.cgroup.join("system.slice/containerd.service")).unwrap();
}

/// Without the io controller in a top-level cgroup, or the root's `io.stat`, what no cgroup wrote can't be told:
/// the window is not split, and the rest of its figures stand. A reading that lacks the split leaves its window
/// unsplit, as a restart's first window starts split again.
#[test]
fn a_window_without_every_top_level_cgroup_s_count_is_not_split() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    cgroups(&paths, 1, 1600);
    drive(&paths, 0, 0);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    // A top-level cgroup without the io controller.
    std::fs::create_dir_all(paths.cgroup.join("machine.slice")).unwrap();
    cgroups(&paths, 2, 1600);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    assert_eq!(window.owners, None, "not split");
    assert!(window.slice.is_some(), "the slice's figure stands");
    // With every count again, the window that starts on the unsplit reading stays unsplit, and the next splits.
    std::fs::remove_dir_all(paths.cgroup.join("machine.slice")).unwrap();
    cgroups(&paths, 3, 1600);
    let window = windows.add(disks.read(&paths, &slice, 1_200_000)).unwrap();
    assert_eq!(
        window.owners, None,
        "it started on a reading without the split"
    );
    cgroups(&paths, 4, 1600);
    let window = windows.add(disks.read(&paths, &slice, 1_800_000)).unwrap();
    assert!(window.owners.is_some(), "split again");
    // A restart: a new daemon's windows start from its first reading.
    let mut restarted = Windows::default();
    restarted.add(disks.read(&paths, &slice, 1_900_000));
    std::fs::remove_file(paths.cgroup.join("io.stat")).unwrap();
    let window = restarted
        .add(disks.read(&paths, &slice, 2_500_000))
        .unwrap();
    assert_eq!(window.owners, None, "the root's count gone");
}

/// A restarted service starts its counters again while its slice keeps what it wrote: taking the difference of
/// lifetime counters moved the dead service's lifetime bytes into "other system services" in one tick. A top-level
/// cgroup that went did the same to no cgroup's share.
#[test]
fn a_restarted_service_or_a_gone_cgroup_moves_no_bytes_between_owners() {
    const K: u64 = 1024;
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    cgroups(&paths, 1, 1600);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    // Docker restarted: its cgroup is new, at 10K; system.slice kept its 300K and wrote 300K more.
    cgroups(&paths, 2, 1600);
    io_stat_of(
        &paths.cgroup.join("system.slice/docker.service"),
        10 * K,
        10,
    );
    let owners = windows
        .add(disks.read(&paths, &slice, 600_000))
        .unwrap()
        .owners
        .unwrap();
    assert_eq!(owners.docker, Some(10 * K), "the new cgroup's count from 0");
    assert_eq!(
        owners.system,
        (300 - 10 - 30 + 10) * K,
        "system.slice's rise less Docker's and containerd's, and init.scope's"
    );
    // init.scope went: what it wrote before is the root's already, and its rise is nothing, not its lifetime.
    std::fs::remove_dir_all(paths.cgroup.join("init.scope")).unwrap();
    cgroups(&paths, 3, 1600);
    std::fs::remove_dir_all(paths.cgroup.join("init.scope")).unwrap();
    let owners = windows
        .add(disks.read(&paths, &slice, 1_200_000))
        .unwrap()
        .owners
        .unwrap();
    // The root rose by 490K + 1600K; the children left by 100K + 300K + 80K.
    assert_eq!(owners.kernel, (490 + 1600 - 480) * K);
}

/// The owner's Docker root is an ext4 in a file on btrfs, on loop0: runnerd, the services and btrfs write the drive,
/// not the loop device, so reading the loop's lines showed every writer but Docker at 0 and the 16K inference compared
/// one file system's writes with another's commits.
#[test]
fn on_a_root_in_a_file_the_split_reads_the_physical_drive() {
    const K: u64 = 1024;
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let lo = paths.sys.join("devices/virtual/block/loop0");
    std::fs::create_dir_all(lo.join("loop")).unwrap();
    std::fs::write(lo.join("dev"), "7:0\n").unwrap();
    std::fs::write(
        lo.join("loop/backing_file"),
        "/var/lib/runnerd/docker.img\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join("class/block/loop0")).unwrap();
    std::os::unix::fs::symlink(&lo, paths.sys.join("dev/block/7:0")).unwrap();
    let mountinfo = paths.proc.join("self/mountinfo");
    let mut mounts = std::fs::read_to_string(&mountinfo).unwrap();
    mounts.push_str("50 40 7:0 / /var/lib/docker rw,relatime shared:9 - ext4 /dev/loop0 rw\n");
    std::fs::write(&mountinfo, mounts).unwrap();
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    assert_eq!(disks.store().as_deref(), Some("loop0"));
    let slice = paths.slice("ci.slice");
    // Docker's daemon writes the store on loop0 as well as the drive: only the drive's line is its share.
    let docker_store = |step: u64| {
        let path = paths.cgroup.join("system.slice/docker.service/io.stat");
        let stat = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!(
                "{stat}7:0 rbytes=0 wbytes={} rios=0 wios=1 dbytes=0 dios=0\n",
                999 * K * step
            ),
        )
        .unwrap();
    };
    cgroups(&paths, 1, 1600);
    docker_store(1);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    cgroups(&paths, 2, 1600);
    docker_store(2);
    let window = windows.add(disks.read(&paths, &slice, 600_000)).unwrap();
    let owners = window.owners.expect("split on the drive");
    assert_eq!(owners.docker, Some(120 * K));
    assert_eq!(owners.runnerd, Some(20 * K), "runnerd writes the drive");
    assert_eq!(owners.user, 60 * K);
    assert_eq!(
        owners.kernel_write(),
        Some(16 * K),
        "btrfs's writes on the drive"
    );
}

/// One tick that fails to read a running service's `io.stat` leaves the next tick nothing to take its rise from:
/// counted from 0, it put the service's lifetime since boot into one window.
#[test]
fn a_service_read_once_without_its_count_takes_no_more_than_its_slice_rose() {
    const K: u64 = 1024;
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    // Ten steps since boot: Docker at 1200K.
    cgroups(&paths, 10, 1600);
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    std::fs::remove_file(paths.cgroup.join("system.slice/docker.service/io.stat")).unwrap();
    assert_eq!(windows.add(disks.read(&paths, &slice, 200_000)), None);
    cgroups(&paths, 11, 1600);
    let owners = windows
        .add(disks.read(&paths, &slice, 600_000))
        .unwrap()
        .owners
        .unwrap();
    assert!(
        owners.docker.is_some_and(|docker| docker <= 300 * K),
        "Docker's rise at most system.slice's 300K, not its 1320K since boot: {owners:?}"
    );
}

// Catches the CPU of the first sample after a sleep divided by the hours asleep, which reads as an idle container
// (ADR 0159).
#[test]
fn after_a_sleep_cpu_is_use_over_the_time_awake() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    container(&paths, "a", 10, 4_000, 0, 1_000_000);
    let mut state = Sampler::new(paths.clone(), "ci.slice");
    let runners = [tracked("runner-a", "a", 10)];
    state.sample(&runners, 10_000);
    // The host slept an hour; awake, the container used 3 CPU seconds in the 2 s before the next sample.
    state.slept(3_600_000);
    container(&paths, "a", 10, 4_000, 0, 4_000_000);
    let after = state.sample(&runners, 10_000 + 3_600_000 + 2_000).samples;
    assert_eq!(after[0].cpu, 1500);
}

// Catches a drive's window stretched across a sleep the sampler did not see, so what the drive wrote while the host
// was down, or during a resume, counts as written in the minutes before it (ADR 0159).
#[test]
fn a_reading_long_after_the_last_starts_a_new_drive_window() {
    let dir = TestDir::new("observe");
    let paths = paths(&dir.0);
    disk_host(&paths, "btrfs");
    let disks = Disks::resolve(&paths, Path::new("/var/lib/docker")).unwrap();
    let slice = paths.slice("ci.slice");
    std::fs::create_dir_all(&slice).unwrap();
    let mut windows = Windows::default();
    windows.add(disks.read(&paths, &slice, 0));
    drive(&paths, 1500, 0);
    assert_eq!(windows.add(disks.read(&paths, &slice, 300_000)), None);
    drive(&paths, 9000, 0);
    let closed = windows
        .add(disks.read(&paths, &slice, 300_000 + 3_600_000))
        .expect("the gap ends the window");
    assert_eq!(
        (closed.start, closed.end, closed.drive),
        (0, 300_000, Some(500 * 512)),
        "the window ends at the last reading before the gap, without what rose across it"
    );
    drive(&paths, 9100, 0);
    let next = windows.add(disks.read(&paths, &slice, 300_000 + 3_600_000 + 600_000));
    assert_eq!(
        next.map(|window| window.drive),
        Some(Some(100 * 512)),
        "the next window starts after the gap"
    );
}

#[test]
fn only_a_system_battery_that_discharges_puts_the_host_on_battery() {
    use super::power::{Power, read};
    // Each host: its supplies as (name, type, scope, status), `None` for a file the kernel leaves out.
    type Supplies<'a> = &'a [(&'a str, &'a str, Option<&'a str>, Option<&'a str>)];
    let hosts: [(&str, Supplies<'_>, Power); 9] = [
        // A desktop: nothing listed, or no directory at all.
        ("desktop", &[], Power::Mains),
        ("missing", &[], Power::Mains),
        // A laptop that charges only over USB-C has no Mains supply: plugged in, its battery charges.
        (
            "usb-c",
            &[
                ("ucsi-source-psy-1", "USB", None, Some("Charging")),
                ("BAT0", "Battery", None, Some("Charging")),
            ],
            Power::Mains,
        ),
        // Plugged in and held at a charge limit, or full.
        (
            "limit",
            &[("BAT0", "Battery", None, Some("Not charging"))],
            Power::Mains,
        ),
        (
            "full",
            &[("BAT0", "Battery", Some("System"), Some("Full"))],
            Power::Mains,
        ),
        // A mouse's battery is the mouse's.
        (
            "mouse",
            &[
                ("AC", "Mains", None, None),
                (
                    "hidpp_battery_0",
                    "Battery",
                    Some("Device"),
                    Some("Discharging"),
                ),
            ],
            Power::Mains,
        ),
        // A battery whose status cannot be read says nothing.
        ("unread", &[("BAT0", "Battery", None, None)], Power::Mains),
        // Unplugged: the battery runs the host, whatever another battery says.
        (
            "unplugged",
            &[
                ("BAT0", "Battery", None, Some("Discharging")),
                ("BAT1", "Battery", Some("System"), Some("Not charging")),
            ],
            Power::Battery,
        ),
        (
            "system",
            &[("BAT0", "Battery", Some("System"), Some("Discharging"))],
            Power::Battery,
        ),
    ];
    let dir = TestDir::new("power");
    for (host, supplies, power) in hosts {
        let sys = dir.0.join(host);
        if host != "missing" {
            std::fs::create_dir_all(sys.join("class/power_supply")).unwrap();
        }
        for (name, kind, scope, status) in supplies {
            let supply = sys.join("class/power_supply").join(name);
            std::fs::create_dir_all(&supply).unwrap();
            std::fs::write(supply.join("type"), format!("{kind}\n")).unwrap();
            for (file, value) in [("scope", scope), ("status", status)] {
                if let Some(value) = value {
                    std::fs::write(supply.join(file), format!("{value}\n")).unwrap();
                }
            }
        }
        assert_eq!(read(&sys), power, "{host}");
    }
}

/// The capture script, run by bash over the fake diagnostic directory `diag`, from `start` on, with `bin` first on its
/// `PATH`; stdin is empty, as under `docker exec`.
struct Capture {
    child: std::process::Child,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    read: Vec<u8>,
}

impl Capture {
    fn start(diag: &Path, bin: &Path, start: LogPosition) -> Capture {
        let script = super::capture::SCRIPT.replace(super::capture::DIAG, &diag.to_string_lossy());
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = std::process::Command::new("bash")
            .args(["-c", &script, "capture"])
            .args([start.page.to_string(), start.offset.to_string()])
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (send, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(read @ 1..) = std::io::Read::read(&mut stdout, &mut buffer) {
                if send.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Capture {
            child,
            output,
            read: Vec::new(),
        }
    }

    /// Everything printed so far, once it holds `wanted`; fails after 10 s without it.
    fn until(&mut self, wanted: &str) -> &[u8] {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !String::from_utf8_lossy(&self.read).contains(wanted) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.output.recv_timeout(left) {
                Ok(chunk) => self.read.extend(chunk),
                Err(error) => panic!(
                    "no {wanted:?} in {:?}: {error}",
                    String::from_utf8_lossy(&self.read)
                ),
            }
        }
        &self.read
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A runner's diagnostic directory whose worker took job 7 of run 8, as job record `abc-1`, and `bin`, which holds
/// `sleep` and `grep` stand-ins when asked.
fn diag(dir: &Path, worker_log: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let diag = dir.join("_diag");
    let bin = dir.join("bin");
    std::fs::create_dir_all(diag.join("pages")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(diag.join("Worker_20261124.log"), worker_log).unwrap();
    (diag, bin)
}

const TAKEN: &str = "\"k\": \"check_run_id\",\n\"v\": 7.0\n\"k\": \"run_id\",\n\"v\": \"8\"\n\"jobId\": \"abc-1\"\n";

/// Puts an executable `name` in `bin` that appends a line to `bin/<name>.calls` and prints nothing.
fn stand_in(bin: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = bin.join(name);
    let calls = bin.join(format!("{name}.calls"));
    std::fs::write(&path, format!("#!/bin/sh\necho >> '{}'\n", calls.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn calls(bin: &Path, name: &str) -> usize {
    std::fs::read_to_string(bin.join(format!("{name}.calls")))
        .map_or(0, |calls| calls.lines().count())
}

#[test]
fn the_capture_script_follows_a_page_until_it_goes_and_resumes_mid_page_without_forking_sleep() {
    let dir = TestDir::new("capture-script");
    let (diag, bin) = diag(&dir.0, TAKEN);
    stand_in(&bin, "sleep");
    let page = diag.join("pages/tl_abc-1_1.log");
    std::fs::write(&page, "first\nsecond\n").unwrap();

    let mut capture = Capture::start(&diag, &bin, LogPosition { page: 1, offset: 0 });
    capture.until("@@page 1 0 13\n");
    // The runner appends to the page, then uploads and deletes it: the hard link keeps what came last.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&page)
        .and_then(|mut page| std::io::Write::write_all(&mut page, b"third\n"))
        .unwrap();
    capture.until("@@page 1 13 6\n");
    std::fs::remove_file(&page).unwrap();
    let output = capture.until("@@end 1\n").to_vec();
    drop(capture);
    let captured = feed_all(&[&output]);
    assert_eq!(captured[0], Captured::Job { id: 7, run_id: 8 });
    assert_eq!(lines(&captured), ["first", "second", "third"]);
    assert!(matches!(
        captured.last(),
        Some(Captured::Lines {
            position: LogPosition { page: 2, offset: 0 },
            ..
        })
    ));

    // A restarted daemon resumes after the stored "first" line, and the kept link of an older page goes.
    std::fs::write(diag.join("pages/tl_abc-1_2.log"), "first\nsecond\n").unwrap();
    std::fs::write(diag.join(".runnerd-pages/tl_abc-1_1.log"), "old\n").unwrap();
    let mut capture = Capture::start(&diag, &bin, LogPosition { page: 2, offset: 6 });
    let output = capture.until("@@page 2 6 7\nsecond\n").to_vec();
    drop(capture);
    assert_eq!(lines(&feed_all(&[&output])), ["second"]);
    assert!(
        !diag.join(".runnerd-pages/tl_abc-1_1.log").exists(),
        "a page before the resumed one is dropped"
    );
    assert_eq!(
        calls(&bin, "sleep"),
        0,
        "the script waits without forking sleep"
    );
}

#[test]
fn an_idle_capture_script_waits_between_passes_rather_than_spinning() {
    let dir = TestDir::new("capture-idle");
    // A worker log with no job yet: each pass greps it three times, and finds nothing.
    let (diag, bin) = diag(
        &dir.0,
        "Waiting to receive the job message from the channel.\n",
    );
    stand_in(&bin, "grep");
    stand_in(&bin, "sleep");
    let capture = Capture::start(&diag, &bin, LogPosition { page: 1, offset: 0 });
    std::thread::sleep(std::time::Duration::from_secs(1));
    drop(capture);
    let passes = calls(&bin, "grep") / 3;
    // Five a second; a `read` that returned at once, as at EOF, would make thousands.
    assert!(
        (2..=8).contains(&passes),
        "{passes} passes in a second, about five expected"
    );
    assert_eq!(
        calls(&bin, "sleep"),
        0,
        "the script waits without forking sleep"
    );
}
