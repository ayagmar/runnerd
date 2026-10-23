//! Parsing `runnerd.toml` and checking it against a slice's budget.

use std::path::Path;

use super::{ByteSize, Config, Image, Warning, parse_memory_max};

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
fn example_config_fits_the_owners_slice() {
    // 33G as `systemctl show ci.slice -p MemoryMax --value` prints it.
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
        matches!(warnings.as_slice(), [Warning::UnlimitedSlice { slice, .. }] if slice == "ci.slice"),
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
fn example_config_pulls_this_releases_images() {
    let version = env!("CARGO_PKG_VERSION");
    for (name, profile) in example().profiles {
        assert_eq!(
            profile.image,
            Image::Pulled(format!("ghcr.io/ayagmar/runnerd-runner:{name}-{version}")),
            "examples/runnerd.toml's {name} profile"
        );
    }
}
