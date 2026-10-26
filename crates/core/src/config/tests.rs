//! Parsing `runnerd.toml` and checking it against a slice's budget.

use std::path::Path;
use std::time::Duration;

use super::{
    BuildCacheWriters, ByteSize, Config, Image, Warning, keep_secrets, mask_secrets, parse_age,
    parse_memory_max, parse_slice_cpus,
};

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
        ("web", format!("{MINIMAL}\n[web]\ntypo = 1")),
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
        (
            "sizing.interval",
            format!("{MINIMAL}\n[sizing]\ninterval = \"0s\""),
        ),
        (
            "sizing.max_step",
            format!("{MINIMAL}\n[sizing]\nmax_step = 0"),
        ),
        (
            "profiles.app.max_slots",
            MINIMAL.replace("slots = 1", "slots = 1\nmax_slots = 0"),
        ),
        (
            "caches.build.max_size",
            MINIMAL.replace(
                "maven = { max_size = \"5G\" }",
                "maven = { max_size = \"5G\" }\nbuild = { max_size = \"0G\" }",
            ),
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
fn a_config_from_before_the_build_cache_gives_every_repository_one_main_writes() {
    let config = Config::parse(MINIMAL).unwrap();
    let build = &config.caches.build;
    assert!(build.enabled, "on: its defaults are safe");
    assert!(
        build.branches.is_empty(),
        "only each repository's default branch writes"
    );
    assert_eq!(build.max_size, size("10G"));
    assert!(config.repositories[0].build_cache, "owner/app gets it");
    assert_eq!(
        config.repositories[0].build_cache_writers,
        BuildCacheWriters::DefaultBranch,
        "no pull request writes unless the repository says so"
    );
}

#[test]
fn auto_sizing_is_off_and_bounded_unless_configured() {
    let config = Config::parse(MINIMAL).unwrap();
    assert!(
        !config.sizing.auto,
        "auto sizing stays off unless asked for"
    );
    assert_eq!(config.sizing.interval, Duration::from_hours(1));
    assert_eq!(config.sizing.max_step, 50);
    // app has 4G and 1 slot.
    for (bounds, why) in [
        (
            "min_memory = \"6G\"\nmax_memory = \"5G\"",
            "min_memory is above its max_memory",
        ),
        ("max_memory = \"2G\"", "memory is above its max_memory"),
        ("min_memory = \"6G\"", "memory is below its min_memory"),
        ("slots = 2\nmax_slots = 1", "slots are above its max_slots"),
    ] {
        let text = MINIMAL.replace("slots = 1", &format!("slots = 1\n{bounds}"));
        let text = text.replace("slots = 1\nslots = 2", "slots = 2");
        let error = Config::parse(&text)
            .unwrap()
            .validate(Some(size("33G")))
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("profiles.app: {why}"));
    }
}

fn origins(list: &str) -> Result<Vec<String>, String> {
    Config::parse(&format!("{MINIMAL}\n[web]\norigins = [{list}]\n"))
        .map(|config| config.web.origins.iter().map(ToString::to_string).collect())
        .map_err(|error| error.to_string())
}

#[test]
fn the_dashboard_takes_writes_from_no_other_origin_unless_configured() {
    let config = Config::parse(MINIMAL).unwrap();
    assert!(config.web.origins.is_empty(), "no extra origins by default");
}

#[test]
fn the_dashboard_opens_on_one_of_its_origins_or_the_config_is_refused() {
    let web = |lines: &str| {
        Config::parse(&format!("{MINIMAL}\n[web]\n{lines}\n"))
            .unwrap()
            .validate(None)
            .map_err(|error| error.to_string())
    };
    assert_eq!(
        Config::parse(MINIMAL).unwrap().web.open,
        None,
        "loopback by default"
    );
    let tailnet = r#"origins = ["https://forge.tail0b5e.ts.net:8443"]"#;
    web(&format!(
        "{tailnet}\nopen = \"https://forge.tail0b5e.ts.net:8443\""
    ))
    .unwrap();
    let error = web(&format!(
        "{tailnet}\nopen = \"https://other.tail0b5e.ts.net\""
    ))
    .unwrap_err();
    assert!(
        error.contains("https://other.tail0b5e.ts.net")
            && error.contains("not one of [web] origins"),
        "{error}"
    );
    web(r#"open = "https://forge.tail0b5e.ts.net:8443""#).unwrap_err();
}

/// Each input and what `new URL(input).origin` gives for it in Node 24, which follows the WHATWG URL standard as
/// browsers do: the `Origin` header a page at that address sends.
const BROWSER_ORIGINS: [(&str, &str); 11] = [
    (
        "https://Host.Example.ts.net:443",
        "https://host.example.ts.net",
    ),
    ("https://host.example.:8443", "https://host.example.:8443"),
    ("http://localhost:8080", "http://localhost:8080"),
    ("http://[0:0:0:0:0:0:0:1]:8443", "http://[::1]:8443"),
    ("https://[2001:0DB8::1]:8443", "https://[2001:db8::1]:8443"),
    (
        "https://[2001:db8:0:0:1:0:0:1]",
        "https://[2001:db8::1:0:0:1]",
    ),
    ("https://[1:0:0:2:0:0:0:3]", "https://[1:0:0:2::3]"),
    ("https://[0:0:1:0:0:0:0:0]", "https://[0:0:1::]"),
    (
        "https://[::ffff:127.0.0.1]:8443",
        "https://[::ffff:7f00:1]:8443",
    ),
    ("http://127.0.0.1.:7477", "http://127.0.0.1:7477"),
    ("https://192.0.2.1:8443", "https://192.0.2.1:8443"),
];

#[test]
fn an_origin_is_kept_as_the_browser_sends_it() {
    for (input, browser) in BROWSER_ORIGINS {
        assert_eq!(
            origins(&format!("{input:?}")),
            Ok(vec![browser.to_owned()]),
            "{input}"
        );
    }
}

#[test]
fn an_ipv4_address_in_another_form_than_four_decimals_is_refused() {
    // Browsers read each as 127.0.0.1 or 8.0.0.1; written that way they would be a second spelling to get wrong.
    for input in [
        "https://2130706433:8443",
        "https://0x7f.1",
        "http://127.1:7477",
        "https://127.000.0.1",
        "https://010.0.0.1",
        "https://example.0x",
    ] {
        let error = origins(&format!("{input:?}")).unwrap_err();
        assert!(error.contains("four decimal numbers"), "{input}: {error}");
    }
}

#[test]
fn an_origin_with_a_path_plain_http_or_no_port_number_is_refused() {
    for (origin, why) in [
        ("https://host.example.ts.net:8443/", "it has a path"),
        ("https://host.example/dashboard", "it has a path"),
        (
            "http://host.example.ts.net:8443",
            "http is for a loopback host only",
        ),
        ("host.example.ts.net:8443", "it has no scheme"),
        ("ftp://host.example", "its scheme is not https"),
        ("https://host.example:http", "its port is not a number"),
        ("https://host.example:0", "its port is not a number"),
        ("https://me@host.example", "it names a user"),
        ("https://", "its host is not a name"),
    ] {
        let error = origins(&format!("{origin:?}")).unwrap_err();
        assert!(error.contains(why), "{origin}: {error}");
        assert!(
            error.contains("origins = ["),
            "{origin} shows the line: {error}"
        );
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
            cpus: None,
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
    let cpus = Config::parse(&text).unwrap().profiles["app"].cpus;
    assert_eq!(profile.cpus, cpus, "a resize without cpus leaves them");
    let with_cpus = super::resize_profile(
        &text,
        "app",
        crate::api::Resize {
            memory: 4 << 30,
            slots: 1,
            cpus: Some(cpus + 2),
        },
    )
    .unwrap();
    assert_eq!(
        Config::parse(&with_cpus).unwrap().profiles["app"].cpus,
        cpus + 2
    );
}

#[test]
fn resizing_a_profile_the_config_lacks_names_it() {
    let error = super::resize_profile(
        MINIMAL,
        "jvm",
        crate::api::Resize {
            memory: 1 << 30,
            slots: 1,
            cpus: None,
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
    // The check names the rule, and the figure it holds to MemoryMax is one runner, not every cap.
    assert_eq!(check.admission, crate::api::AdmissionPolicy::Measured);
    let max = size("10G").0;
    assert!(
        check.memory_caps > Some(max),
        "the caps add up past MemoryMax"
    );
    assert!(
        check.memory_needed.is_some_and(|needed| needed <= max),
        "one runner fits: {check:?}"
    );
    // The same caps admitted by caps do not fit, and there the caps are what must fit.
    let caps = MINIMAL.replace("slots = 1", "slots = 8");
    let refused = Config::check(&caps, |_| Ok(Some(size("10G")))).1;
    assert!(!refused.valid, "8 × 4G of caps do not fit 10G");
    assert_eq!(refused.memory_needed, refused.memory_caps);
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
fn measured_admission_without_a_profile_still_needs_the_mirror_to_fit() {
    let text = r#"
repositories = []

[host]
slice = "ci.slice"
isolation = "sysbox"
admission = "measured"

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles]
"#;
    let (_, check) = Config::check(text, |_| Ok(Some(size("256M"))));
    assert!(
        !check.valid,
        "a 512M mirror does not fit 256M: {}",
        check.message
    );
    assert!(check.message.contains("512M"), "{}", check.message);
    let (_, check) = Config::check(text, |_| Ok(Some(size("1G"))));
    assert!(check.valid, "a 512M mirror fits 1G: {}", check.message);
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

#[test]
fn cpu_admission_is_off_unless_an_overcommit_above_0_is_set() {
    assert_eq!(Config::parse(MINIMAL).unwrap().host.cpu_overcommit, None);
    let with = |factor: &str| {
        MINIMAL.replace(
            "isolation = \"sysbox\"",
            &format!("isolation = \"sysbox\"\ncpu_overcommit = {factor}"),
        )
    };
    let config = Config::parse(&with("1.5")).unwrap();
    assert_eq!(config.host.cpu_overcommit, Some(1.5));
    assert!(config.validate(Some(size("10G"))).is_ok());
    for factor in ["0", "-1.0", "17", "nan"] {
        let error = Config::parse(&with(factor))
            .unwrap()
            .validate(Some(size("10G")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("host.cpu_overcommit"), "{factor}: {error}");
    }
}

#[test]
fn a_slices_cpus_are_its_cpu_set_or_the_hosts_less_under_a_lower_quota() {
    let show = |effective: &str, allowed: &str, quota: &str| {
        format!("EffectiveCPUs={effective}\nAllowedCPUs={allowed}\nCPUQuotaPerSecUSec={quota}\n")
    };
    assert_eq!(
        parse_slice_cpus(&show("", "", "infinity"), 32),
        Some(32_000)
    );
    assert_eq!(
        parse_slice_cpus(&show("0-11,16", "", "infinity"), 32),
        Some(13_000)
    );
    assert_eq!(
        parse_slice_cpus(&show("", "0-3", "infinity"), 32),
        Some(4000)
    );
    // A 250% quota: two and a half CPUs' time each second.
    assert_eq!(
        parse_slice_cpus(&show("0-31", "", "2.500000s"), 32),
        Some(2500)
    );
    assert_eq!(parse_slice_cpus(&show("", "", "800ms"), 32), Some(800));
    assert_eq!(parse_slice_cpus(&show("0-3", "", "8s"), 32), Some(4000));
    // As systemctl prints disjoint ranges, and a 6400% and a 10000% quota.
    assert_eq!(
        parse_slice_cpus(&show("0-3 8-11", "", "infinity"), 32),
        Some(8000)
    );
    assert_eq!(
        parse_slice_cpus(&show("", "0 2 4-5", "infinity"), 32),
        Some(4000)
    );
    assert_eq!(
        parse_slice_cpus(&show("", "", "1min 4s"), 128),
        Some(64_000)
    );
    assert_eq!(
        parse_slice_cpus(&show("", "", "1min 40s"), 128),
        Some(100_000)
    );
    assert_eq!(parse_slice_cpus(&show("zero", "", "infinity"), 32), None);
    assert_eq!(parse_slice_cpus(&show("", "", "4 parsecs"), 32), None);
}

const NOTIFY: &str = r#"
[notify]
digest_at = "07:30"

[[notify.targets]]
name = "phone"
kind = "ntfy"
url = "https://ntfy.example/halyard-ci-6f3a"
headers = { Authorization = "Bearer tk_hunter2" }

[[notify.targets]]
name = "relay"
kind = "webhook"
url = "https://hooks.example/runnerd?key=s3cret"
events = ["failed", "digest"]
"#;

#[test]
fn notify_targets_get_failures_lost_jobs_runners_and_insights_by_default() {
    use crate::api::NotifyEvent;

    let config = Config::parse(&format!("{MINIMAL}{NOTIFY}")).unwrap();

    assert!(config.validate(None).is_ok());
    assert_eq!(config.notify.digest_at.to_string(), "07:30");
    let events: Vec<_> = config
        .notify
        .targets
        .iter()
        .map(|target| target.events.clone())
        .collect();
    assert_eq!(
        events,
        [
            vec![
                NotifyEvent::Failed,
                NotifyEvent::Lost,
                NotifyEvent::Runner,
                NotifyEvent::Insights
            ],
            vec![NotifyEvent::Failed, NotifyEvent::Digest],
        ]
    );
    assert!(
        Config::parse(MINIMAL).unwrap().notify.targets.is_empty(),
        "nothing is sent without [notify]"
    );
}

#[test]
fn a_notify_targets_url_and_headers_never_show_in_debug_output_or_errors() {
    let config = Config::parse(&format!("{MINIMAL}{NOTIFY}")).unwrap();
    let shown = format!("{config:?}");
    for secret in ["halyard-ci-6f3a", "hunter2", "s3cret", "hooks.example"] {
        assert!(!shown.contains(secret), "Debug shows {secret}: {shown}");
    }

    let broken = NOTIFY.replace(
        "https://hooks.example/runnerd?key=s3cret",
        "hooks.example/runnerd?key=s3cret",
    );
    let error = Config::parse(&format!("{MINIMAL}{broken}"))
        .unwrap()
        .validate(None)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "notify target \"relay\": url does not start with https:// or http://"
    );
}

#[test]
fn an_ntfy_target_needs_a_topic_and_targets_need_their_own_names() {
    let refused = |notify: &str| {
        Config::parse(&format!("{MINIMAL}{notify}"))
            .unwrap()
            .validate(None)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        refused(&NOTIFY.replace("ntfy.example/halyard-ci-6f3a", "ntfy.example/")),
        "notify target \"phone\": url is not an ntfy topic's, such as https://ntfy.sh/<topic>"
    );
    assert_eq!(
        refused(&NOTIFY.replace("\"relay\"", "\"phone\"")),
        "notify target \"phone\": another target has this name"
    );
    assert!(
        Config::parse(&format!("{MINIMAL}{}", NOTIFY.replace("07:30", "7:30"))).is_err(),
        "a time of day is HH:MM"
    );
}

#[test]
fn a_config_shown_with_its_notify_secrets_masked_saves_with_them_kept() {
    // Headers as a table of their own too, with the targets swapped round in the edit.
    let stored = format!(
        "{MINIMAL}{}\n[notify.targets.headers]\nX-Key = \"k3y\"\n",
        NOTIFY.replace("events = [\"failed\", \"digest\"]\n", "")
    );

    let shown = mask_secrets(&stored).unwrap();

    for secret in ["halyard-ci-6f3a", "hunter2", "s3cret", "k3y"] {
        assert!(!shown.contains(secret), "shows {secret}: {shown}");
    }
    assert_eq!(
        mask_secrets(MINIMAL).as_deref(),
        Some(MINIMAL),
        "a config without targets is shown as written"
    );
    assert_eq!(keep_secrets(&shown, &stored), stored);
    let edited = shown.replace("07:30", "08:00");
    assert_eq!(
        keep_secrets(&edited, &stored),
        stored.replace("07:30", "08:00")
    );
    let rewritten = shown.replace(
        "url = \"<secret>\"\nheaders",
        "url = \"https://ntfy.example/new-topic\"\nheaders",
    );
    assert!(
        keep_secrets(&rewritten, &stored).contains("https://ntfy.example/new-topic"),
        "a value written over the mask is the new one"
    );
}

/// MINIMAL with a `light` profile serving owner/app's jobs that ask for `labels` (a TOML array), with `extra` lines.
fn with_light_pool(labels: &str, extra: &str) -> Config {
    Config::parse(&format!(
        "{MINIMAL}
[profiles.light]
image_stage = \"app\"
cpus = 1
memory = \"1G\"
slots = 1

[[repositories]]
name = \"owner/app\"
profile = \"light\"
labels = {labels}
{extra}"
    ))
    .unwrap()
}

#[test]
fn a_repository_may_be_served_by_pools_with_labels_of_their_own() {
    let config = with_light_pool("[\"app-light\"]", "");
    assert_eq!(config.validate(Some(size("33G"))).unwrap(), Vec::new());
    let profile = |labels: &[&str]| {
        let labels: Vec<String> = labels.iter().map(|label| (*label).to_owned()).collect();
        config
            .pool("owner/app", &labels)
            .map(|pool| pool.profile.as_str())
    };
    assert_eq!(profile(&["self-hosted"]), Some("app"));
    assert_eq!(profile(&["APP-LIGHT"]), Some("light"));
    assert_eq!(profile(&["self-hosted", "app-light"]), None);
    assert_eq!(config.repository_names(), ["owner/app"]);
}

#[test]
fn pools_of_a_repository_sharing_a_label_are_rejected() {
    // A job asking for `self-hosted` alone could land in either.
    let error = with_light_pool("[\"Self-Hosted\", \"app-light\"]", "")
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("owner/app"), "{error}");
    assert!(error.contains("self-hosted"), "{error}");
    assert!(error.contains("app") && error.contains("light"), "{error}");
}

#[test]
fn a_pool_without_labels_is_rejected() {
    let error = with_light_pool("[]", "")
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("needs at least one non-empty label"),
        "{error}"
    );
}

#[test]
fn two_pools_of_a_repository_in_one_profile_are_rejected() {
    let text = format!(
        "{MINIMAL}
[[repositories]]
name = \"owner/app\"
profile = \"app\"
labels = [\"app-light\"]
"
    );
    let error = Config::parse(&text)
        .unwrap()
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("two entries with profile \"app\""),
        "{error}"
    );
}

#[test]
fn pools_of_a_repository_must_agree_on_who_writes_to_the_build_cache() {
    let error = with_light_pool("[\"app-light\"]", "build_cache_writers = \"pull-requests\"")
        .validate(Some(size("33G")))
        .unwrap_err()
        .to_string();
    assert!(error.contains("build_cache_writers"), "{error}");
}

#[test]
fn a_profile_is_priced_at_the_github_runner_it_names() {
    let config =
        Config::parse(&MINIMAL.replace("slots = 1\n", "slots = 1\npriced_as = \"linux_8_core\"\n"))
            .unwrap();
    assert_eq!(
        config.pricing().of(Some("app")),
        crate::api::Sku::Linux8Core
    );
    assert_eq!(
        config.pricing().of(Some("gone")),
        crate::api::Sku::standard_for(std::env::consts::ARCH),
        "a profile gone from the config is priced at the standard runner"
    );
    for refused in ["windows_4_core", "linux_4_core_gpu", "8-core"] {
        let error = Config::parse(&MINIMAL.replace(
            "slots = 1\n",
            &format!("slots = 1\npriced_as = \"{refused}\"\n"),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("linux_8_core"),
            "{refused}: names the runners it takes: {error}"
        );
    }
}
