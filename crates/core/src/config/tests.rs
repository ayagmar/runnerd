//! Parsing `runnerd.toml` and checking it against a slice's budget.

use std::path::Path;
use std::time::Duration;

use super::{ByteSize, Config, Image, Warning, parse_age, parse_memory_max};

const MINIMAL: &str = r#"
[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "app"
cpus = 4
memory = "4G"
slots = 1

[[repositories]]
name = "owner/app"
profile = "app"
labels = ["self-hosted", "app"]
"#;

fn example() -> Config {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/runnerd.toml");
    Config::load(&path).unwrap()
}

fn size(s: &str) -> ByteSize {
    s.parse().unwrap()
}

#[test]
fn example_config_fits_its_slice() {
    // 33G as `systemctl show runnerd.slice -p MemoryMax --value` prints it.
    let memory_max = parse_memory_max("35433480192").unwrap();
    assert_eq!(example().validate(memory_max).unwrap(), Vec::new());
}

#[test]
fn example_config_over_a_smaller_slice_names_both_numbers() {
    let error = example()
        .validate(Some(size("30G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("32.5G"), "{error}");
    assert!(error.contains("30G"), "{error}");
}

#[test]
fn config_with_unknown_profile_names_the_repository() {
    let text = MINIMAL.replace("profile = \"app\"", "profile = \"missing\"");
    let error = Config::parse(&text)
        .unwrap()
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("owner/app"), "{error}");
    assert!(error.contains("missing"), "{error}");
}

#[test]
fn config_with_unsafe_dind_requires_allow_unsafe() {
    let unsafe_dind = MINIMAL.replace("isolation = \"sysbox\"", "isolation = \"unsafe-dind\"");
    let error = Config::parse(&unsafe_dind)
        .unwrap()
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("allow_unsafe"), "{error}");

    let allowed = unsafe_dind.replace("[host]", "[host]\nallow_unsafe = true");
    Config::parse(&allowed)
        .unwrap()
        .validate(Some(size("33G")))
        .unwrap();
}

#[test]
fn config_with_unlimited_slice_validates_with_a_warning() {
    let memory_max = parse_memory_max("infinity").unwrap();
    let warnings = example().validate(memory_max).unwrap();
    assert!(
        matches!(warnings.as_slice(), [Warning::UnlimitedSlice { slice, .. }] if slice == "runnerd.slice"),
        "{warnings:?}"
    );
}

#[test]
fn config_rejects_unknown_fields_in_every_table() {
    for (table, with_typo) in [
        ("top level", format!("typo = 1\n{MINIMAL}")),
        ("host", MINIMAL.replace("[host]", "[host]\ntypo = 1")),
        ("caches", MINIMAL.replace("[caches]", "[caches]\ntypo = 1")),
        (
            "mirror",
            MINIMAL.replace("ttl = \"168h\"", "ttl = \"168h\", typo = 1"),
        ),
        ("pnpm", MINIMAL.replace("pnpm = {", "pnpm = { typo = 1,")),
        (
            "profile",
            MINIMAL.replace("slots = 1", "slots = 1\ntypo = 1"),
        ),
        (
            "repository",
            MINIMAL.replace("profile = \"app\"", "profile = \"app\"\ntypo = 1"),
        ),
        ("records", format!("{MINIMAL}\n[records]\ntypo = 1")),
    ] {
        let error = Config::parse(&with_typo).unwrap_err().to_string();
        assert!(error.contains("unknown field `typo`"), "{table}: {error}");
    }
}

#[test]
fn config_with_a_zero_limit_names_the_field() {
    let mirror_enabled = MINIMAL.replace("enabled = false", "enabled = true");
    for (field, text) in [
        (
            "profiles.app.memory",
            MINIMAL.replace("memory = \"4G\"", "memory = \"0G\""),
        ),
        ("profiles.app.cpus", MINIMAL.replace("cpus = 4", "cpus = 0")),
        (
            "profiles.app.slots",
            MINIMAL.replace("slots = 1", "slots = 0"),
        ),
        (
            "caches.mirror.memory",
            mirror_enabled.replace("memory = \"512M\"", "memory = \"0M\""),
        ),
        (
            "caches.mirror.cpus",
            mirror_enabled.replace("cpus = 1,", "cpus = 0,"),
        ),
        (
            "records.max_count",
            format!("{MINIMAL}\n[records]\nmax_count = 0"),
        ),
    ] {
        let error = Config::parse(&text)
            .unwrap()
            .validate(Some(size("33G")))
            .unwrap_err()
            .to_string();
        assert!(error.contains(field), "{field}: {error}");
    }
}

#[test]
fn records_are_off_and_bounded_unless_configured() {
    let config = Config::parse(MINIMAL).unwrap();
    assert!(
        !config.profiles["app"].record_failures,
        "a profile records no failures unless it asks to"
    );
    assert_eq!(config.records.max_total, size("20G"));
    assert_eq!(config.records.max_count, 10);
    assert_eq!(config.records.max_age, Duration::from_hours(7 * 24));

    let text = MINIMAL.replace("slots = 1", "slots = 1\nrecord_failures = true")
        + "\n[records]\nmax_total = \"5G\"\nmax_age = \"2d\"\n";
    let config = Config::parse(&text).unwrap();
    assert!(
        config.profiles["app"].record_failures,
        "app records failures"
    );
    assert_eq!(config.records.max_total, size("5G"));
    assert_eq!(config.records.max_age, Duration::from_hours(48));
    assert_eq!(
        config.records.max_count, 10,
        "unset limits keep their default"
    );
}

#[test]
fn config_rejects_a_size_beyond_u64() {
    let text = MINIMAL.replace("memory = \"4G\"", "memory = \"16777216T\"");
    let error = Config::parse(&text).unwrap_err().to_string();
    assert!(error.contains("too large"), "{error}");
}

#[test]
fn config_whose_caps_overflow_is_refused() {
    // 2 × 2^63 bytes does not fit in u64; a saturating sum would pass against the largest slice.
    let text = MINIMAL
        .replace("memory = \"4G\"", "memory = \"8388608T\"")
        .replace("slots = 1", "slots = 2");
    let error = Config::parse(&text)
        .unwrap()
        .validate(Some(ByteSize(u64::MAX)))
        .unwrap_err()
        .to_string();
    assert!(error.contains("too large"), "{error}");
}

#[test]
fn config_rejects_a_size_without_unit() {
    let text = MINIMAL.replace("memory = \"4G\"", "memory = \"4\"");
    let error = Config::parse(&text).unwrap_err().to_string();
    assert!(error.contains("\"4\""), "{error}");
}

#[test]
fn config_rejects_empty_labels() {
    let text = MINIMAL.replace("labels = [\"self-hosted\", \"app\"]", "labels = []");
    let error = Config::parse(&text)
        .unwrap()
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("owner/app"), "{error}");
}

#[test]
fn profile_runs_a_local_stage_or_a_pulled_reference() {
    let local = Config::parse(MINIMAL).unwrap();
    assert_eq!(
        local.profiles["app"].image.reference(),
        "runnerd-runner:app"
    );
    let text = MINIMAL.replace(
        "image_stage = \"app\"",
        "image = \"ghcr.io/ayagmar/runnerd-runner:jvm-0.0.5\"",
    );
    let pulled = Config::parse(&text).unwrap();
    assert_eq!(
        pulled.profiles["app"].image,
        Image::Pulled("ghcr.io/ayagmar/runnerd-runner:jvm-0.0.5".to_owned())
    );
}

#[test]
fn profile_needs_exactly_one_image() {
    for (case, text) in [
        ("neither", MINIMAL.replace("image_stage = \"app\"\n", "")),
        (
            "both",
            MINIMAL.replace(
                "image_stage = \"app\"",
                "image_stage = \"app\"\nimage = \"ghcr.io/ayagmar/runnerd-runner:jvm-0.0.5\"",
            ),
        ),
        (
            "empty",
            MINIMAL.replace("image_stage = \"app\"", "image = \"\""),
        ),
    ] {
        let error = Config::parse(&text).unwrap_err().to_string();
        assert!(error.contains("set image_stage"), "{case}: {error}");
    }
}

#[test]
fn example_config_pulls_published_images_no_older_than_this_release() {
    let numbers = |version: &str| -> Vec<u32> {
        version
            .split('-')
            .next()
            .unwrap()
            .split('.')
            .map(|part| part.parse().unwrap())
            .collect()
    };
    let release = numbers(env!("CARGO_PKG_VERSION"));
    for (name, profile) in example().profiles {
        let Image::Pulled(reference) = &profile.image else {
            panic!("examples/runnerd.toml's {name} profile does not pull its image");
        };
        let tag = reference
            .strip_prefix(&format!("ghcr.io/ayagmar/runnerd-runner:{name}-"))
            .unwrap_or_else(|| panic!("{name} runs {reference}, not its published stage"));
        assert!(
            numbers(tag) >= release,
            "{name} runs {reference}, older than this release; bump the example's tags"
        );
    }
}

#[test]
fn a_resized_profile_keeps_the_rest_of_the_file_as_written() {
    let text = MINIMAL.replace(
        "memory = \"4G\"\nslots = 1",
        "memory = \"4G\" # peaked at 3.1G\nslots = 1",
    );
    let resized = super::resize_profile(
        &format!("# the app's runners\n{text}"),
        "app",
        crate::api::Resize {
            memory: 2560 << 20,
            slots: 3,
        },
    )
    .unwrap();
    assert!(resized.starts_with("# the app's runners\n"), "{resized}");
    assert!(
        resized.contains("memory = \"2.5G\" # peaked at 3.1G\nslots = 3\n"),
        "{resized}"
    );
    let profile = &Config::parse(&resized).unwrap().profiles["app"];
    assert_eq!((profile.memory, profile.slots), (size("2.5G"), 3));
}

#[test]
fn resizing_a_profile_the_config_lacks_names_it() {
    let error = super::resize_profile(
        MINIMAL,
        "jvm",
        crate::api::Resize {
            memory: 1 << 30,
            slots: 1,
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the config has no profile \"jvm\" under [profiles]"
    );
}

/// `MINIMAL` under measured admission with `slots` slots.
fn measured(slots: u32) -> String {
    MINIMAL
        .replace(
            "isolation = \"sysbox\"",
            "isolation = \"sysbox\"\nadmission = \"measured\"",
        )
        .replace("slots = 1", &format!("slots = {slots}"))
}

#[test]
fn measured_admission_lets_the_caps_add_up_past_memory_max() {
    // 8 × 4G = 32G of caps on a 10G slice: one runner fits, the predictions decide how many more.
    let (config, check) = Config::check(&measured(8), |_| Ok(Some(size("10G"))));
    assert!(check.valid, "{}", check.message);
    assert!(check.message.contains("measured"), "{}", check.message);
    assert_eq!(config.unwrap().host.admission, super::Admission::Measured);
    // The same caps admitted by caps do not fit.
    let caps = MINIMAL.replace("slots = 1", "slots = 8");
    assert!(!Config::check(&caps, |_| Ok(Some(size("10G")))).1.valid);
}

#[test]
fn measured_admission_needs_one_runner_of_every_profile_to_fit() {
    let error = Config::parse(&measured(2))
        .unwrap()
        .validate(Some(size("3G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("one runner of app"), "{error}");
}

#[test]
fn measured_admission_needs_a_memory_max_to_admit_against() {
    let error = Config::parse(&measured(2))
        .unwrap()
        .validate(None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("has none"), "{error}");
}

#[test]
fn an_age_reads_in_seconds_to_weeks_and_names_its_units_when_it_does_not() {
    assert_eq!(parse_age("30m"), Ok(Duration::from_mins(30)));
    assert_eq!(parse_age("30d"), Ok(Duration::from_hours(30 * 24)));
    assert_eq!(parse_age("2w"), Ok(Duration::from_hours(14 * 24)));
    for wrong in ["30", "d", "3x", "1.5d", ""] {
        assert_eq!(
            parse_age(wrong),
            Err("expected a number and s, m, h, d or w, such as 12h".to_owned()),
            "{wrong:?}"
        );
    }
}
