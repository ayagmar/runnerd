//! `init` on made-up machines, writing into temp dirs.

use std::path::Path;

use runnerd_core::config::{ByteSize, Config, Image};

use super::{Machine, Overwrite, Request, init, size};
use crate::testdir::Dir;

const GIB: u64 = 1 << 30;

fn machine(memory_gib: u64, cpus: u32, slice_memory_max: Option<ByteSize>) -> Machine {
    Machine {
        memory: ByteSize(memory_gib * GIB),
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
fn every_sizing_fits_its_budget_and_three_quarters_of_the_host() {
    for memory_gib in 1..=256 {
        for cpus in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 64, 128] {
            for slice_gib in [None, Some(4), Some(33), Some(1024)] {
                let machine = machine(memory_gib, cpus, slice_gib.map(|gib| ByteSize(gib * GIB)));
                let Ok(sizing) = size(&machine) else {
                    assert!(
                        memory_gib < 4,
                        "a {memory_gib}G host in a {slice_gib:?}G slice gets a config"
                    );
                    continue;
                };
                let room = sizing.budget.0 - super::MIRROR_MEMORY.0;
                assert!(
                    sizing.budget.0 <= machine.memory.0 / 4 * 3,
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
    let sizing = size(&machine(4, 32, Some(ByteSize(64 * GIB)))).unwrap();
    assert_eq!(sizing.budget, ByteSize(3 * GIB), "three quarters of 4G");
    assert_eq!(sizing.slots, 1);
}

#[test]
fn a_nominal_4g_vm_gets_a_runner() {
    let machine = Machine {
        memory: ByteSize(3_900_000 << 10),
        cpus: 2,
        slice_memory_max: None,
    };
    let sizing = size(&machine).unwrap();
    assert_eq!(sizing.memory, ByteSize(2 * GIB));
    assert_eq!(sizing.budget, ByteSize(5 << 29), "2.5G");
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
