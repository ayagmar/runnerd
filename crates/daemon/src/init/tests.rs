//! `init` on made-up machines, writing into temp dirs.

use std::path::Path;

use runnerd_core::admission::host;
use runnerd_core::api::HostMemory;
use runnerd_core::config::{ByteSize, Config, Image};

use super::{Machine, Overwrite, Request, init, labels, runs_on, size};
use crate::testdir::Dir;

const GIB: u64 = 1 << 30;

/// A host with `total` bytes of memory, `available` of them free, and nothing in the slice.
fn host_memory(total: u64, available: u64) -> HostMemory {
    HostMemory {
        at: 0,
        total,
        available,
        slice: 0,
        some: None,
        full: None,
        holders: Vec::new(),
        ceiling: None,
    }
}

/// A host of `memory_gib` with an eighth of it in use, as an idle server with Docker.
fn machine(memory_gib: u64, cpus: u32, slice_memory_max: Option<ByteSize>) -> Machine {
    let total = memory_gib * GIB;
    Machine {
        memory: host_memory(total, total / 8 * 7),
        cpus,
        slice_memory_max,
    }
}

fn request(dir: &Path, overwrite: Overwrite) -> Request {
    Request {
        repositories: vec!["halyard/ledger".to_owned(), "halyard/storefront".to_owned()],
        slice: "runnerd.slice".to_owned(),
        runnerd: "/home/dana/.local/bin/runnerd".into(),
        config: dir.join("runnerd/runnerd.toml"),
        unit: dir.join("systemd/user/runnerd.service"),
        overwrite,
        memory_ceiling: None,
    }
}

fn check(text: &str, memory_max: Option<ByteSize>) -> runnerd_core::api::ConfigCheck {
    Config::check(text, |_| Ok(memory_max)).1
}

#[test]
fn the_starter_config_passes_check_config_before_and_after_the_slice_exists() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let sizing = init(&request, &machine(64, 16, None)).unwrap();
    assert_eq!(sizing.budget, ByteSize(48 * GIB), "three quarters of 64G");
    let text = std::fs::read_to_string(&request.config).unwrap();
    let before = check(&text, None);
    assert!(before.valid, "{}", before.message);
    let after = check(&text, Some(sizing.budget));
    assert!(after.valid, "{}", after.message);
    assert!(after.warnings.is_empty(), "{:?}", after.warnings);
    let config = Config::parse(&text).unwrap();
    let names: Vec<&str> = config
        .repositories
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, ["halyard/ledger", "halyard/storefront"]);
    assert_eq!(config.host.slice, "runnerd.slice");
    assert!(
        config.sizing.auto,
        "a new config has auto sizing on, so nobody sizes profiles by hand"
    );
    assert_eq!(
        config.profiles["default"].image,
        Image::Pulled(format!(
            "ghcr.io/ayagmar/runnerd-runner:base-{}",
            env!("CARGO_PKG_VERSION")
        )),
        "the starter profile pulls this release's published base stage"
    );
}

#[test]
fn the_starter_config_fits_the_memory_max_of_an_existing_slice() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let memory_max = ByteSize(20 * GIB);
    let sizing = init(&request, &machine(128, 32, Some(memory_max))).unwrap();
    assert_eq!(sizing.budget, memory_max);
    let text = std::fs::read_to_string(&request.config).unwrap();
    let check = check(&text, Some(memory_max));
    assert!(check.valid, "{}", check.message);
}

#[test]
fn every_sizing_fits_its_budget_three_quarters_of_the_host_and_what_admission_leaves_its_first_runner()
 {
    for memory_gib in 1..=256 {
        for cpus in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 64, 128] {
            for slice_gib in [None, Some(4), Some(33), Some(1024)] {
                let machine = machine(memory_gib, cpus, slice_gib.map(|gib| ByteSize(gib * GIB)));
                let Ok(sizing) = size(&machine) else {
                    assert!(
                        memory_gib < 6,
                        "a {memory_gib}G host in a {slice_gib:?}G slice gets a config"
                    );
                    continue;
                };
                let room = sizing.budget.0 - super::MIRROR_MEMORY.0;
                assert!(
                    sizing.memory.0 + super::STARTUP_HEADROOM
                        <= host::first_runner(&machine.memory, super::MIRROR_MEMORY.0),
                    "{memory_gib}G: the first runner fits the host with room for what starts after init: {sizing:?}"
                );
                assert!(
                    sizing.budget.0 <= machine.memory.total / 4 * 3,
                    "{memory_gib}G, slice {slice_gib:?}G: {sizing:?}"
                );
                assert!(
                    u64::from(sizing.slots) * sizing.memory.0 <= room,
                    "{memory_gib}G, {cpus} CPUs: {sizing:?}"
                );
                assert!(
                    sizing.slots >= 1 && sizing.cpus >= 1,
                    "{memory_gib}G, {cpus} CPUs: {sizing:?}"
                );
            }
        }
    }
}

#[test]
fn a_slice_larger_than_the_host_does_not_size_past_the_host() {
    let sizing = size(&machine(8, 32, Some(ByteSize(64 * GIB)))).unwrap();
    assert_eq!(sizing.budget, ByteSize(6 * GIB), "three quarters of 8G");
    assert_eq!(sizing.slots, 1);
}

/// A first job waits for the host's memory forever when init sized its runner past what admission leaves it once
/// runnerd runs: no job finishes, so auto sizing has nothing to lower it from.
#[test]
fn the_first_job_of_a_starter_config_is_admitted_on_a_small_host() {
    let mut waiting = Vec::new();
    // `MemTotal` and `MemAvailable` of Ubuntu 24.04 VMs with Docker running and nothing else, in kB.
    for (name, total, available) in [
        ("4G", 4_005_036, 3_412_108),
        ("6G", 6_034_412, 5_390_220),
        ("8G", 8_131_800, 7_402_516),
        ("16G", 16_370_412, 15_520_188),
    ] {
        let dir = Dir::new();
        let request = request(&dir.0, Overwrite::Refuse);
        let machine = Machine {
            memory: host_memory(total << 10, available << 10),
            cpus: 4,
            slice_memory_max: None,
        };
        let sizing = match init(&request, &machine) {
            Ok(sizing) => sizing,
            Err(error) => {
                assert_eq!(name, "4G", "{name}: {error}");
                assert!(
                    error.contains("this host has 3.25G of 3.82G available")
                        && error.contains("512M for what starts with it")
                        && error.contains("a runner needs at least 2G"),
                    "{error}"
                );
                assert!(!request.config.exists(), "{name}: no config is written");
                continue;
            }
        };
        let config = Config::load(&request.config).unwrap();
        let runner = config.profiles["default"].memory.0;
        // As the engine admits the first runner into an empty slice, the host's budget beside the mirror's cap, once
        // the daemon, containerd with the unpacked image and the mirror's shim hold some of what init saw.
        let started = HostMemory {
            available: machine.memory.available - (256 << 20),
            ..machine.memory.clone()
        };
        let reserved = config.caches.mirror.memory.0;
        let budget = host::budget(started, reserved, host::Remembered::default());
        if runner > budget.spare {
            waiting.push(format!(
                "{name}: a {} runner waits, the host leaves it {}",
                ByteSize(runner),
                ByteSize(budget.spare)
            ));
        }
        let checked = config.check_against(Some(sizing.budget));
        assert!(checked.valid, "{name}: {}", checked.message);
    }
    assert!(waiting.is_empty(), "every first job starts: {waiting:?}");
}

/// The idle Hyper-V VM of #159: 2.12G with 1.48G available.
fn idle_vm(ceiling: Option<u64>) -> Machine {
    Machine {
        memory: HostMemory {
            ceiling,
            ..host_memory((2 << 30) + (120 << 20), (1 << 30) + (490 << 20))
        },
        cpus: 8,
        slice_memory_max: None,
    }
}

#[test]
fn init_refuses_an_idle_elastic_vm_it_reads_as_it_is_now() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let error = init(&request, &idle_vm(None)).unwrap_err();
    assert!(
        error.contains("this host has 1.48G of 2.12G available")
            && error.contains("a runner needs at least 2G"),
        "{error}"
    );
    assert!(!request.config.exists(), "no config is written");
}

#[test]
fn a_reading_with_a_ceiling_sizes_by_first_runner_as_admission_counts_it() {
    let machine = idle_vm(Some(24 * GIB));
    let sizing = size(&machine).unwrap();
    assert_eq!(sizing.budget, ByteSize(18 * GIB), "three quarters of 24G");
    assert_eq!((sizing.slots, sizing.memory), (2, ByteSize(8 * GIB)));
    assert!(
        sizing.memory.0 + super::STARTUP_HEADROOM
            <= host::first_runner(&machine.memory, super::MIRROR_MEMORY.0),
        "the first runner fits what admission leaves it with the ceiling"
    );
}

/// `request` with `--memory-ceiling ceiling`.
fn with_ceiling(request: Request, ceiling: &str) -> Request {
    Request {
        memory_ceiling: Some(ceiling.to_owned()),
        ..request
    }
}

#[test]
fn init_given_a_memory_ceiling_writes_it_and_sizes_the_first_runner_for_the_host_as_it_can_grow() {
    let dir = Dir::new();
    let request = with_ceiling(request(&dir.0, Overwrite::Refuse), "24G");
    // The idle VM init refuses without a ceiling.
    let machine = idle_vm(None);
    let sizing = init(&request, &machine).unwrap();
    assert_eq!(sizing.budget, ByteSize(18 * GIB), "three quarters of 24G");
    assert_eq!((sizing.slots, sizing.memory), (2, ByteSize(8 * GIB)));
    let grown = HostMemory {
        ceiling: Some(24 * GIB),
        ..machine.memory.clone()
    };
    assert!(
        sizing.memory.0 + super::STARTUP_HEADROOM
            <= host::first_runner(&grown, super::MIRROR_MEMORY.0),
        "the first runner fits what admission leaves it on the host as it can grow"
    );
    let text = std::fs::read_to_string(&request.config).unwrap();
    assert!(
        text.contains("for a host with 2.12G, growing to 24G and 8 CPUs")
            && text.contains("memory_ceiling = \"24G\""),
        "{text}"
    );
    let checked = check(&text, None);
    assert!(checked.valid, "{}", checked.message);
    let config = Config::parse(&text).unwrap();
    assert_eq!(config.host.memory_ceiling, Some(ByteSize(24 * GIB)));
}

#[test]
fn a_memory_ceiling_the_config_check_refuses_writes_and_replaces_nothing() {
    let dir = Dir::new();
    let forced = request(&dir.0, Overwrite::Replace);
    for path in [&forced.config, &forced.unit] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "kept").unwrap();
    }
    // A 64G host sizes 8G runners; a 4G ceiling under MemTotal does not change that, but no host of 4G starts one.
    let cases = [
        (
            "4G",
            "host.memory_ceiling is 4G, under the 8.5G one runner of default and the mirror take",
        ),
        ("0G", "host.memory_ceiling is 0; set it above 0"),
        ("24", "--memory-ceiling: invalid size \"24\""),
    ];
    for (ceiling, refused) in cases {
        let request = with_ceiling(request(&dir.0, Overwrite::Replace), ceiling);
        let error = init(&request, &machine(64, 16, None)).unwrap_err();
        assert!(error.starts_with(refused), "{ceiling}: {error}");
    }
    // 0 is refused as 0 even on a host too small for a runner, where sizing would fail first.
    let zero = with_ceiling(request(&dir.0, Overwrite::Replace), "0G");
    let error = init(&zero, &idle_vm(None)).unwrap_err();
    assert!(error.starts_with("host.memory_ceiling is 0"), "{error}");
    for path in [&forced.config, &forced.unit] {
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "kept",
            "{}",
            path.display()
        );
    }
}

#[test]
fn a_host_too_small_for_one_runner_gets_no_config() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let error = init(&request, &machine(2, 4, None)).unwrap_err();
    assert!(error.contains("a runner needs at least 2G"), "{error}");
    assert!(!request.config.exists(), "no config is written");
}

#[test]
fn the_config_is_readable_by_its_user_only_under_a_default_umask() {
    // Process-wide: put back as it was, for the tests running beside this one.
    let umask = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let written = init(&request, &machine(64, 16, None));
    rustix::process::umask(umask);
    written.unwrap();
    let mode = |path: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777
    };
    assert_eq!(mode(&request.config), 0o600, "the config");
    assert_eq!(
        mode(request.config.parent().unwrap()),
        0o700,
        "its directory"
    );
    assert_eq!(mode(&request.unit), 0o644, "the unit is not secret");
}

#[test]
fn init_refuses_to_replace_a_file_without_force() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    std::fs::create_dir_all(request.unit.parent().unwrap()).unwrap();
    std::fs::write(&request.unit, "mine").unwrap();
    let error = init(&request, &machine(64, 16, None)).unwrap_err();
    assert!(error.contains("--force"), "{error}");
    assert_eq!(std::fs::read_to_string(&request.unit).unwrap(), "mine");
    assert!(!request.config.exists(), "the config is not written either");

    let forced = Request {
        overwrite: Overwrite::Replace,
        ..request
    };
    init(&forced, &machine(64, 16, None)).unwrap();
    assert!(
        std::fs::read_to_string(&forced.unit)
            .unwrap()
            .contains("ExecStart="),
        "--force replaces the unit"
    );
}

#[test]
fn the_unit_serves_the_written_config_with_this_binary() {
    let dir = Dir::new();
    let request = Request {
        runnerd: "/opt/run nerd/bin/runnerd".into(),
        config: dir.0.join("100%/runnerd.toml"),
        ..request(&dir.0, Overwrite::Refuse)
    };
    init(&request, &machine(64, 16, None)).unwrap();
    let unit = std::fs::read_to_string(&request.unit).unwrap();
    let exec_start = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .unwrap();
    assert_eq!(
        exec_start,
        format!(
            "\"/opt/run nerd/bin/runnerd\" serve --config \"{}/100%%/runnerd.toml\"",
            dir.0.display()
        )
    );
    assert!(unit.contains("WantedBy=default.target"), "{unit}");
}

#[test]
fn init_rejects_a_repository_that_is_not_owner_slash_name() {
    let dir = Dir::new();
    for name in ["ledger", "halyard/ledger/x", "halyard/", "halyard/le\"dger"] {
        let request = Request {
            repositories: vec![name.to_owned()],
            ..request(&dir.0, Overwrite::Refuse)
        };
        let error = init(&request, &machine(64, 16, None)).unwrap_err();
        assert!(error.contains("owner/name"), "{name}: {error}");
    }
    assert!(!dir.0.join("runnerd").exists(), "nothing is written");
}

#[test]
fn the_slice_fix_adds_a_drop_in_rather_than_replace_an_existing_slice_unit() {
    let fix = super::slice_fix(
        "ci.slice",
        ByteSize(33 * GIB),
        Some("/etc/systemd/system/ci.slice"),
    );
    assert!(
        fix.contains("sudo tee /etc/systemd/system/ci.slice.d/runnerd.conf"),
        "{fix}"
    );
    let fix = super::slice_fix("ci.slice", ByteSize(33 * GIB), None);
    assert!(
        fix.contains("sudo tee /etc/systemd/system/ci.slice >"),
        "{fix}"
    );
    assert!(fix.contains("MemoryMax=33G"), "{fix}");
}

#[test]
fn an_apostrophe_in_a_path_stays_inside_the_quotes() {
    let dir = Dir::new();
    let request = Request {
        config: dir.0.join("o'neal/runnerd.toml"),
        ..request(&dir.0, Overwrite::Refuse)
    };
    init(&request, &machine(64, 16, None)).unwrap();
    let unit = std::fs::read_to_string(&request.unit).unwrap();
    assert!(
        unit.contains(&format!(
            "--config \"{}/o'neal/runnerd.toml\"\n",
            dir.0.display()
        )),
        "{unit}"
    );
}

#[test]
fn a_unit_that_cannot_be_written_leaves_no_config() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    // The unit's parent directory cannot be created: `systemd` is a file.
    std::fs::write(dir.0.join("systemd"), "").unwrap();
    let error = init(&request, &machine(64, 16, None)).unwrap_err();
    assert!(error.contains("runnerd.service"), "{error}");
    let left: Vec<_> = std::fs::read_dir(dir.0.join("runnerd")).unwrap().collect();
    assert!(left.is_empty(), "no config or temp file is left: {left:?}");
}

#[test]
fn a_failed_forced_init_keeps_the_old_config() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Replace);
    std::fs::create_dir_all(request.config.parent().unwrap()).unwrap();
    std::fs::write(&request.config, "mine").unwrap();
    // Publishing the unit fails after the config is in place: its path is a directory that is not empty.
    std::fs::create_dir_all(request.unit.join("x")).unwrap();
    init(&request, &machine(64, 16, None)).unwrap_err();
    assert_eq!(std::fs::read_to_string(&request.config).unwrap(), "mine");
    let left: Vec<_> = std::fs::read_dir(request.config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["runnerd.toml"], "no temp or backup file is left");
}

#[test]
fn init_refuses_a_destination_that_is_not_a_regular_file_even_with_force() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Replace);
    std::fs::create_dir_all(request.config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(dir.0.join("elsewhere")).unwrap();
    for target in ["nowhere", "elsewhere"] {
        let _ = std::fs::remove_file(&request.config);
        std::os::unix::fs::symlink(dir.0.join(target), &request.config).unwrap();
        let error = init(&request, &machine(64, 16, None)).unwrap_err();
        assert!(error.contains("not a regular file"), "{target}: {error}");
        assert_eq!(
            std::fs::read_link(&request.config).unwrap(),
            dir.0.join(target),
            "the symlink to {target} is kept"
        );
        assert!(!request.unit.exists(), "the unit is not written either");
    }
}

#[test]
fn a_failed_init_leaves_another_runs_staged_file_alone() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    let config_dir = request.config.parent().unwrap();
    std::fs::create_dir_all(config_dir).unwrap();
    let sentinel = config_dir.join(format!(".runnerd.toml.runnerd-init-{}", std::process::id()));
    std::fs::write(&sentinel, "another run's").unwrap();
    std::fs::write(dir.0.join("systemd"), "").unwrap();
    init(&request, &machine(64, 16, None)).unwrap_err();
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "another run's");
    let left: Vec<_> = std::fs::read_dir(config_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(left, [sentinel], "only the other run's file is left");
}

#[test]
fn a_staging_name_that_exists_is_skipped_not_reused() {
    let dir = Dir::new();
    let mut taken = Vec::new();
    let (path, ()) = super::unique(&dir.0, "runnerd.toml", |path| {
        if taken.len() < 3 {
            taken.push(path.to_owned());
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        Ok(())
    })
    .unwrap();
    assert!(!taken.contains(&path), "{path:?} was taken");
}

#[test]
fn the_printed_runs_on_line_asks_for_exactly_the_labels_init_wrote() {
    let dir = Dir::new();
    let request = request(&dir.0, Overwrite::Refuse);
    init(&request, &machine(64, 16, None)).unwrap();
    let config = Config::load(&request.config).unwrap();
    let hint = runs_on(&labels());
    let asked: Vec<&str> = hint
        .lines()
        .find_map(|line| line.trim().strip_prefix("runs-on: ["))
        .and_then(|list| list.strip_suffix(']'))
        .unwrap()
        .split(", ")
        .collect();
    for repository in &config.repositories {
        assert_eq!(asked, repository.labels, "{}", repository.name);
    }
}
