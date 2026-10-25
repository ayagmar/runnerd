//! The previous release and this one read each other's JSON: what the daemon sends its clients, what clients send
//! it, what the history stores and what `[notify]` webhooks receive. runnerctl, the TUI and the dashboard ship with
//! the daemon, but an upgrade leaves an older client talking to the new daemon, or a new client to the daemon still
//! running, and a downgraded daemon reads the history a newer one wrote.
//!
//! Both ways, with no running daemon:
//!
//! - The previous release reads what this one writes: every sample in `samples` parses as the previous release's
//!   type, from `runnerd-core-previous`, the dev-dependency pinned to its commit, and written back it has every key
//!   and value of the sample it read. That catches a removed or renamed field, defaulted and optional ones too, an
//!   enum variant its readers have no fallback for or read as their fallback, and a changed representation.
//! - This one reads what the previous release wrote: `fixtures/`, which that release's samples wrote, one file for
//!   every document, parse as this version's types with the values they hold. That catches a newly required field
//!   and a removed variant.
//!
//! A break made on purpose is one line in [`ACCEPTED`], naming the ADR that records it. An entry that no longer
//! fails fails the check too, so the list stays true. CONTRIBUTING.md says how to move both to a new release.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use runnerd_core::api;
use runnerd_core_previous::api as previous;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

mod samples;

/// The release `runnerd-core-previous` is pinned to, whose samples `fixtures/` holds.
const PREVIOUS: &str = "v0.0.13";

/// Which side reads the other's JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// The previous release reads what this one writes.
    Previous,
    /// This version reads what the previous release wrote.
    Current,
}

/// A break between [`PREVIOUS`] and this version that an ADR accepts.
#[derive(Debug)]
struct Accepted {
    reader: Reader,
    /// The document and item that fail to parse, `document/item`.
    item: &'static str,
    adr: &'static str,
}

const ACCEPTED: &[Accepted] = &[Accepted {
    reader: Reader::Previous,
    item: "waiting/no-pool",
    adr: "0058",
}];

/// A document's items as JSON, by label.
type Items = BTreeMap<String, Value>;

/// One JSON document and how each side reads it.
struct Document {
    name: &'static str,
    items: Items,
    current: fn(&Value) -> Result<Value, String>,
    previous: fn(&Value) -> Result<Value, String>,
}

/// What a reader makes of `value`, written back as JSON.
fn reads<T: DeserializeOwned + Serialize>(value: &Value) -> Result<Value, String> {
    T::deserialize(value)
        .map(json)
        .map_err(|error| format!("does not parse: {error}"))
}

/// Which keys of what a reader writes back the JSON it read must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keys {
    /// Every one: a full sample sets every field, so a key it lacks is a field the reader filled in with its
    /// default, one the writer renamed or removed.
    Every,
    /// Those both have: an empty sample leaves fields out, and a field newer than the JSON is the reader's own.
    Shared,
}

/// Where what a reader `read` and wrote back differs from what was `written`, beyond keys only the writer has: a
/// key [`Keys`] says it must have and lacks, or another value, such as a fallback variant's tag.
fn misread(read: &Value, written: &Value, keys: Keys, path: &str) -> Option<String> {
    match (read, written) {
        (Value::Object(read), Value::Object(written)) => read.iter().find_map(|(key, value)| {
            let path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            match written.get(key) {
                Some(written) => misread(value, written, keys, &path),
                None if keys == Keys::Every => {
                    Some(format!("reads `{path}`, which it was not sent"))
                }
                None => None,
            }
        }),
        (Value::Array(read), Value::Array(written)) if read.len() == written.len() => read
            .iter()
            .zip(written)
            .enumerate()
            .find_map(|(index, (read, written))| {
                misread(read, written, keys, &format!("{path}[{index}]"))
            }),
        _ if read == written => None,
        _ => Some(format!("reads `{path}` as {read}, sent {written}")),
    }
}

/// Whether `reader` reads `written` and sees what it says.
fn check(
    reader: fn(&Value) -> Result<Value, String>,
    written: &Value,
    keys: Keys,
) -> Result<(), String> {
    let read = reader(written)?;
    misread(&read, written, keys, "").map_or(Ok(()), Err)
}

fn json(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap()
}

/// A full sample and, where serializing skips fields, one that leaves them out.
fn full(sample: impl Serialize, empty: Option<Value>) -> Items {
    let mut items = Items::from([("full".to_owned(), json(sample))]);
    if let Some(empty) = empty {
        items.insert("empty".to_owned(), empty);
    }
    items
}

/// One item per variant, labelled with its name in the JSON: the string itself, or its `kind` or `type` tag.
fn variants<T: Serialize>(every: Vec<T>) -> Items {
    every
        .into_iter()
        .map(|variant| {
            let value = json(variant);
            let label = match &value {
                Value::String(name) => name.clone(),
                Value::Object(fields) => fields
                    .get("kind")
                    .or_else(|| fields.get("type"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned(),
                _ => panic!("{value} is no enum variant"),
            };
            (label, value)
        })
        .collect()
}

macro_rules! documents {
    ($($name:literal: $type:ident => $items:expr,)*) => {
        fn documents() -> Vec<Document> {
            vec![$(Document {
                name: $name,
                items: $items,
                current: reads::<api::$type>,
                previous: reads::<previous::$type>,
            },)*]
        }
    };
}

documents! {
    // Responses.
    "status": Status => full(samples::status(), Some(json(samples::status_empty()))),
    "waiting": Waiting => variants(samples::waiting()),
    "hold-cause": HoldCause => variants(samples::hold_cause()),
    "admission-policy": AdmissionPolicy => variants(samples::admission_policy()),
    "busy": Busy => full(samples::busy(), None),
    "paused": Paused => full(samples::paused(), None),
    "config-file": ConfigFile => full(samples::config_file(), None),
    "config-check": ConfigCheck => full(samples::config_check(), None),
    "doctor": Doctor => full(samples::doctor(), None),
    "dashboard": Dashboard => full(samples::dashboard(), Some(json(api::Dashboard { open: None }))),
    "pairings": Pairings => full(samples::pairings(), None),
    "job-page": JobPage => full(samples::job_page(), None),
    "job-state": JobState => variants(samples::job_state()),
    "job": Job => full(samples::job(), Some(json(samples::job_empty()))),
    "admission": Admission => full(samples::admission(), Some(json(samples::admission_empty()))),
    "basis": Basis => variants(samples::basis()),
    "job-peak": JobPeak => variants(samples::job_peak()),
    "record-removal": RecordRemoval => variants(samples::record_removal()),
    "sample-series": SampleSeries => full(samples::sample_series(), None),
    "host-samples": HostSamples => full(samples::host_samples(), None),
    "stats": Stats => full(samples::stats(), None),
    "bucket": Bucket => variants(samples::bucket()),
    "activity": Activity => full(samples::activity(), None),
    "sizing": Sizing => full(samples::sizing(), Some(json(samples::sizing_empty()))),
    "insights": Insights => full(samples::insights(), None),
    "severity": Severity => variants(samples::severity()),
    "auto-note": AutoNote => variants(samples::auto_note()),
    "digest": Digest => full(samples::digest(), None),
    "notify-test": NotifyTest => full(samples::notify_test(), None),
    "caches": Caches => full(samples::caches(), None),
    "cache-history": CacheHistory => full(samples::cache_history(), None),
    "records": Records => full(samples::records(), None),
    "removed-records": RemovedRecords => full(samples::removed_records(), None),
    "error": ErrorResponse => full(samples::error_response(), None),
    // `/v1/events`, and the durable events the history stores and reads back.
    "events": EventEnvelope => variants(samples::events()),
    // What `[notify]` webhooks receive.
    "notification": Notification => full(samples::notification(), Some(json(samples::notification_empty()))),
    "notify-event": NotifyEvent => variants(samples::notify_event()),
    "priority": Priority => variants(samples::priority()),
    // Requests: bodies, and query strings as the same fields in JSON.
    "shutdown-request": ShutdownRequest => full(samples::shutdown_request(), None),
    "pause-query": PauseQuery => full(samples::pause_query(), None),
    "pairing-query": PairingQuery => full(samples::pairing_query(), None),
    "replace-query": ReplaceQuery => full(samples::replace_query(), None),
    "job-query": JobQuery => full(samples::job_query(), None),
    "log-query": LogQuery => full(samples::log_query(), None),
    "host-sample-query": HostSampleQuery => full(samples::host_sample_query(), None),
    "activity-query": ActivityQuery => full(samples::activity_query(), None),
    "stats-query": StatsQuery => full(samples::stats_query(), None),
    "sizing-query": SizingQuery => full(samples::sizing_query(), None),
    "insights-query": InsightsQuery => full(samples::insights_query(), None),
    "notify-test-query": NotifyTestQuery => full(samples::notify_test_query(), None),
    "cache-history-query": CacheHistoryQuery => full(samples::cache_history_query(), None),
    "event-query": EventQuery => full(samples::event_query(), None),
    "remove-records-query": RemoveRecordsQuery => full(samples::remove_records_query(), None),
    "shell-query": ShellQuery => full(samples::shell_query(), None),
}

/// The previous release's samples, one file per document.
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The previous release's samples of a document, by label.
fn fixture(name: &str) -> Items {
    let text = fs::read_to_string(fixtures().join(format!("{name}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Fails, naming each, on the items that do not parse and no entry accepts, and on the entries for `reader` whose
/// item parses.
fn assert_reads(reader: Reader, failures: &BTreeMap<String, String>) {
    let mut problems = Vec::new();
    for (item, error) in failures {
        if !ACCEPTED
            .iter()
            .any(|accepted| accepted.reader == reader && accepted.item == item)
        {
            problems.push(format!("{item} {error}"));
        }
    }
    for accepted in ACCEPTED.iter().filter(|accepted| accepted.reader == reader) {
        if !failures.contains_key(accepted.item) {
            problems.push(format!(
                "{} parses, so the break ADR {} accepts is gone: remove its entry",
                accepted.item, accepted.adr
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{reader:?} reader against {PREVIOUS}; record a deliberate break in ACCEPTED with its ADR:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_previous_release_reads_what_this_one_writes() {
    // An event of a type the previous release does not have is no break: the client's event stream and the history
    // skip what they cannot read. One it has must still read, renamed or not.
    let known_events = fixture("events");
    let mut failures = BTreeMap::new();
    for document in documents() {
        for (label, value) in &document.items {
            if document.name == "events" && !known_events.contains_key(label) {
                continue;
            }
            let keys = if label == "empty" {
                Keys::Shared
            } else {
                Keys::Every
            };
            if let Err(error) = check(document.previous, value, keys) {
                failures.insert(format!("{}/{label}", document.name), error);
            }
        }
    }
    assert_reads(Reader::Previous, &failures);
}

#[test]
fn this_version_reads_what_the_previous_release_wrote() {
    let documents: BTreeMap<_, _> = documents()
        .into_iter()
        .map(|document| (document.name, document))
        .collect();
    let mut failures = BTreeMap::new();
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        names.insert(name.clone());
        let Some(document) = documents.get(name.as_str()) else {
            failures.insert(name, "is a fixture of no document".to_owned());
            continue;
        };
        for (label, value) in &fixture(&name) {
            if let Err(error) = check(document.current, value, Keys::Shared) {
                failures.insert(format!("{name}/{label}"), error);
            }
        }
    }
    for name in documents.keys().filter(|name| !names.contains(**name)) {
        failures.insert(
            (*name).to_owned(),
            format!("has no fixture: {PREVIOUS} wrote none"),
        );
    }
    assert_reads(Reader::Current, &failures);
}

/// Replaces the fixtures with this version's samples, for the release after it to read. Run when cutting a release
/// (CONTRIBUTING.md).
#[test]
#[ignore = "writes the fixtures of a release being cut"]
fn write_fixtures() {
    let directory = fixtures();
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    fs::create_dir_all(&directory).unwrap();
    for document in documents() {
        let text = serde_json::to_string_pretty(&document.items).unwrap();
        fs::write(
            directory.join(format!("{}.json", document.name)),
            text + "\n",
        )
        .unwrap();
    }
}
