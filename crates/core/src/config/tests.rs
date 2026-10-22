use std::path::Path;

use super::{ByteSize, Config, Warning, parse_memory_max};

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

fn size(s: &str) -> Option<ByteSize> {
    Some(s.parse().unwrap())
}

#[test]
fn example_config_fits_the_owners_slice() {
    // 33G as `systemctl show ci.slice -p MemoryMax --value` prints it.
    let memory_max = parse_memory_max("35433480192").unwrap();
    assert_eq!(example().validate(memory_max).unwrap(), Vec::new());
}

#[test]
fn example_config_over_a_smaller_slice_names_both_numbers() {
    let error = example().validate(size("30G")).unwrap_err().to_string();
    assert!(error.contains("32.5G"), "{error}");
    assert!(error.contains("30G"), "{error}");
}

#[test]
fn config_with_unknown_profile_names_the_repository() {
    let text = MINIMAL.replace("profile = \"app\"", "profile = \"missing\"");
    let error = Config::parse(&text)
        .unwrap()
        .validate(size("33G"))
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
        .validate(size("33G"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("allow_unsafe"), "{error}");

    let allowed = unsafe_dind.replace("[host]", "[host]\nallow_unsafe = true");
    Config::parse(&allowed)
        .unwrap()
        .validate(size("33G"))
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
        .validate(size("33G"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("owner/app"), "{error}");
}
