//! Shares of a host shaped like the one ADR 0156 measured: four profiles of fictional repositories over 33 slots.

use std::collections::BTreeMap;

use super::{StoreReport, StoreUse, needs, slot_shares, store_report};
use crate::cache::docker_budget_gib;
use crate::config::Config;

const G: u64 = 1 << 30;

/// `jvm` 9, `node` 10, `light` 8 and `jvm-small` 6 slots.
fn config(slots: [u32; 4]) -> Config {
    let profile = |name: &str, slots: u32| {
        format!(
            "[profiles.{name}]\nimage_stage = \"base\"\ncpus = 2\nmemory = \"4G\"\nslots = {slots}\n"
        )
    };
    let text = format!(
        "[host]\nslice = \"runnerd.slice\"\nisolation = \"sysbox\"\n\n[caches]\nmirror = {{ enabled = false, memory = \"512M\", cpus = 1, ttl = \"168h\" }}\npnpm = {{ max_size = \"5G\" }}\nmaven = {{ max_size = \"5G\" }}\n\n{}{}{}{}\n[[repositories]]\nname = \"halyard/ledger\"\nprofile = \"jvm\"\nlabels = [\"jvm\"]\n",
        profile("jvm", slots[0]),
        profile("node", slots[1]),
        profile("light", slots[2]),
        profile("jvm-small", slots[3]),
    );
    match Config::parse(&text) {
        Ok(config) => config,
        Err(error) => panic!("the test config parses: {error}"),
    }
}

fn owners() -> Config {
    config([9, 10, 8, 6])
}

fn needing(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
        .iter()
        .map(|(name, need)| ((*name).to_owned(), *need))
        .collect()
}

/// What the shares take of the disk together, in bytes.
fn total(config: &Config, shares: &BTreeMap<String, u64>) -> u64 {
    config
        .profiles
        .iter()
        .map(|(name, profile)| u64::from(profile.slots) * shares[name] * G)
        .sum()
}

#[test]
fn without_a_measured_store_every_slot_keeps_the_equal_share() {
    let config = owners();
    for disk in [0, G / 2, 99 * G, 2000 * G] {
        let shares = slot_shares(disk, &config, &BTreeMap::new());
        for share in shares.values() {
            assert_eq!(
                *share,
                docker_budget_gib(disk, 33),
                "{disk} bytes split equally"
            );
        }
    }
}

#[test]
fn the_profiles_whose_stores_overflow_get_what_the_quiet_ones_do_not_use() {
    // A quarter of 396 GiB over 33 slots is 3 GiB each: jvm's image builds held 4 GiB of that.
    let config = owners();
    let disk = 99 * G;
    let shares = slot_shares(
        disk,
        &config,
        &needing(&[
            ("jvm", 4 * G),
            ("node", 3 * G),
            ("light", G / 3),
            ("jvm-small", G / 2),
        ]),
    );
    assert!(shares["jvm"] >= 5, "jvm's 4G with room to grow: {shares:?}");
    assert!(
        shares["node"] >= 3,
        "node keeps its 3G at least: {shares:?}"
    );
    assert!(
        shares["light"] < 3,
        "light gives up what it never held: {shares:?}"
    );
    assert!(shares["jvm-small"] < 3, "so does jvm-small: {shares:?}");
    assert!(
        total(&config, &shares) <= disk,
        "within the quarter: {shares:?}"
    );
}

#[test]
fn a_profile_not_measured_yet_keeps_the_equal_share_beside_measured_ones() {
    let config = owners();
    let disk = 99 * G;
    let shares = slot_shares(disk, &config, &needing(&[("jvm", 8 * G), ("light", G / 4)]));
    assert_eq!(
        shares["node"], 3,
        "node's image has not reported yet: {shares:?}"
    );
    assert_eq!(shares["jvm-small"], 3, "{shares:?}");
    assert!(shares["jvm"] > 3, "{shares:?}");
}

#[test]
fn a_share_never_passes_20_gib_and_the_disk_left_over_stays_free() {
    let config = config([2, 1, 1, 1]);
    let shares = slot_shares(
        1000 * G,
        &config,
        &needing(&[
            ("jvm", 30 * G),
            ("node", 30 * G),
            ("light", 30 * G),
            ("jvm-small", 30 * G),
        ]),
    );
    assert!(shares.values().all(|share| *share == 20), "{shares:?}");
}

#[test]
fn a_disk_too_small_for_every_slot_still_gives_each_1_gib() {
    let config = owners();
    let shares = slot_shares(10 * G, &config, &needing(&[("jvm", 9 * G), ("light", G)]));
    assert!(shares.values().all(|share| *share == 1), "{shares:?}");
}

/// Splits over generated hosts: a share stays between 1 and 20 GiB, the shares fit the disk whenever it holds 1 GiB a
/// slot, a slot never gets less than the floor, and a profile that needs more never gets less than one that needs
/// less, but for the rounding to whole GiB.
#[test]
fn generated_splits_fit_the_disk_and_follow_the_needs() {
    let mut seed: u64 = 0x5eed_0156;
    let mut next = |below: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % below.max(1)
    };
    let names = ["jvm", "node", "light", "jvm-small"];
    for _ in 0..2000 {
        let slots = [0, 1, 2, 3].map(|_| u32::try_from(next(12)).unwrap_or(0));
        let config = config(slots);
        let count: u64 = slots.iter().map(|slots| u64::from(*slots)).sum();
        let disk = next(800) * G + next(G);
        let mut measured = BTreeMap::new();
        for name in names {
            match next(4) {
                0 => {}
                1 => {
                    measured.insert(name.to_owned(), 0);
                }
                _ => {
                    measured.insert(name.to_owned(), next(30 * G));
                }
            }
        }
        let shares = slot_shares(disk, &config, &measured);
        assert_eq!(shares.len(), 4, "every profile has a share");
        for share in shares.values() {
            assert!(
                (1..=20).contains(share),
                "{share} GiB within 1 and 20: {shares:?}"
            );
        }
        if disk >= count * G {
            assert!(
                total(&config, &shares) <= disk,
                "{shares:?} fit {disk} bytes over {slots:?}"
            );
            let equal = disk / count.max(1);
            let floor = ((equal / 4).max(G).min(equal) / G).max(1);
            for share in shares.values() {
                assert!(*share >= floor.min(20), "{shares:?} keep the floor {floor}");
            }
        }
        for (a, slots_a) in names.iter().zip(slots) {
            for (b, slots_b) in names.iter().zip(slots) {
                if let (Some(need_a), Some(need_b)) = (measured.get(*a), measured.get(*b))
                    && need_a > need_b
                    && slots_a > 0
                    && slots_b > 0
                {
                    // Up to the GiB the rounding gives a profile of fewer slots first.
                    assert!(
                        shares[*a] + 1 >= shares[*b],
                        "{a} needs more than {b}, so it keeps about as much: {shares:?} {measured:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_last_report_of_a_runners_log_is_its_store_and_an_earlier_one_never_counts() {
    let logs = "√ Connected to GitHub\n\
                runnerd: docker store held=99999999999 share=20\n\
                Job build completed with result: Succeeded\n\
                runnerd: docker store held=4294967296 share=3\n";
    let report = store_report(logs);
    assert_eq!(
        report,
        Some(StoreReport {
            held: 4 * G,
            share_gib: 3
        })
    );
    assert!(
        report.is_some_and(|report| report.overflowed()),
        "4G is over a 3G share"
    );
    // A job printed one, and the cleanup measured nothing: the entrypoint's last line wins.
    let forged =
        "runnerd: docker store held=99999999999 share=1\nrunnerd: docker store unmeasured\n";
    assert_eq!(store_report(forged), None);
    // An image from before the report, and lines it cannot read.
    assert_eq!(
        store_report("Job build completed with result: Failed\n"),
        None
    );
    assert_eq!(
        store_report("runnerd: docker store held=lots share=3\n"),
        None
    );
    assert_eq!(
        store_report("runnerd: docker store held=1 share=3 extra\n"),
        None
    );
    assert_eq!(
        store_report("runnerd: docker store held=3221225472 share=3\r\n").map(|r| r.overflowed()),
        Some(false),
        "exactly the share is not over it"
    );
}

#[test]
fn a_profiles_need_is_the_90th_percentile_of_its_last_reports() {
    let uses = |profile: &str, held: &[u64]| -> Vec<StoreUse> {
        held.iter()
            .map(|held| StoreUse {
                profile: profile.to_owned(),
                held: *held,
            })
            .collect()
    };
    let mut all = uses("jvm", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    all.extend(uses("light", &[7, 7, 7, 7]));
    let found = needs(&all);
    assert_eq!(found.get("jvm"), Some(&9));
    assert_eq!(found.get("light"), None, "four reports say too little");
    // Newest first: only the last 100 count, so a store that grew shows within them.
    let mut grown = uses("node", &[50; 100]);
    grown.extend(uses("node", &[1; 400]));
    assert_eq!(needs(&grown).get("node"), Some(&50));
}
