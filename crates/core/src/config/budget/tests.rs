//! What every client says of a checked config: its verdict, and what doctor makes of the same check.

use crate::api::{Check, CheckStatus, Doctor, Tone};
use crate::config::{ByteSize, Config, Verdict};

const CONFIG: &str = r#"
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
slots = 2

[[repositories]]
name = "owner/app"
profile = "app"
labels = ["self-hosted", "app"]
"#;

fn check(text: &str, max: Option<u64>) -> crate::api::ConfigCheck {
    Config::check(text, |_| Ok(max.map(ByteSize))).1
}

/// The verdict on `text` as the dashboard shows it, with the config it parsed.
fn verdict(text: &str, max: Option<u64>) -> Verdict {
    check(text, max).verdict(Config::parse(text).ok().as_ref())
}

#[test]
fn only_a_budget_past_memory_max_does_not_fit() {
    assert_eq!(verdict(CONFIG, Some(8 << 30)), Verdict::Fits);
    assert_eq!(verdict(CONFIG, Some(6 << 30)), Verdict::DoesNotFit);
    assert_eq!(
        check(CONFIG, Some(6 << 30)).verdict(None),
        Verdict::DoesNotFit,
        "from an old daemon's check"
    );
    let budget = check(CONFIG, Some(6 << 30)).budget(None).unwrap();
    let over = budget.remainder().unwrap();
    assert_eq!((over.label, over.memory), ("Over by", 2 << 30));
}

#[test]
fn an_invalid_config_is_not_valid_however_much_memory_is_left() {
    // Plenty of room, but the repository names no profile it has.
    let text = CONFIG.replace("profile = \"app\"", "profile = \"missing\"");
    assert_eq!(verdict(&text, Some(64 << 30)), Verdict::Invalid);
    assert_eq!(verdict(&text, Some(64 << 30)).label(), "Not valid");
    // Its caps are past MemoryMax too, but the missing profile is why it is not valid.
    assert_eq!(verdict(&text, Some(6 << 30)), Verdict::Invalid);
    assert_eq!(verdict("not toml", None), Verdict::Invalid);
}

#[test]
fn a_slice_without_memory_max_holds_no_budget_to_fit() {
    let verdict = verdict(CONFIG, None);
    assert_eq!(verdict, Verdict::NoBudget);
    assert_eq!(verdict.tone(), Tone::Warn, "not proof of a budget");
}

#[test]
fn doctor_says_of_the_config_what_check_config_says() {
    let checked = check(CONFIG, Some(8 << 30));
    let config = Config::parse(CONFIG).unwrap();
    let doctor = config.check_against(Some(ByteSize(8 << 30))).doctor_check();
    assert_eq!(doctor.status, CheckStatus::Pass);
    assert_eq!(doctor.detail, checked.message);
}

#[test]
fn a_warning_is_its_own_tone_and_lets_doctor_pass() {
    let warn = Check {
        name: "sizing".to_owned(),
        status: CheckStatus::Warn,
        detail: String::new(),
        fix: None,
    };
    assert_eq!(warn.status.tone(), Tone::Warn);
    assert!(
        Doctor {
            checks: vec![warn.clone()]
        }
        .passed(),
        "a warning passes"
    );
    let fail = Check {
        status: CheckStatus::Fail,
        ..warn
    };
    assert!(!Doctor::all_passed(&[fail]), "a failure does not");
}

#[test]
fn a_short_size_rounds_to_two_digits_and_keeps_its_unit() {
    let short = |bytes: u64| ByteSize(bytes).short();
    assert_eq!(short(3328 << 20), "3.3G", "3.25G");
    assert_eq!(short(2560 << 20), "2.5G");
    assert_eq!(short(10 << 30), "10G");
    assert_eq!(short(12 << 30), "12G");
    assert_eq!(short(512 << 20), "512M");
    assert_eq!(short(1 << 30), "1G");
    assert_eq!(short((1 << 30) - 1), "1G", "just under 1G");
    assert_eq!(short(2047 << 19), "1G", "1023.5M");
}
