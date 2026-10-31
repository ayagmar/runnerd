//! The job a runner took and the job's log, read from inside its container while it runs. A small script, run with
//! `docker exec` as root, waits for the runner's `_diag/Worker_*.log`, prints the job's ID (`check_run_id`) and run
//! ID from the job message there, then follows the job's log pages in `_diag/pages`, which the runner writes in
//! 4 KiB blocks and deletes once uploaded: the script hard-links each page so none is lost, and prints what it
//! has not printed before. These pages are what GitHub shows, with secrets already masked by the runner.
//!
//! The script's output: `@@job <id> <run id>`, then `@@page <page> <offset> <length>` followed by exactly that
//! many bytes, and `@@end <page>` once a page is complete. `Parser` turns it into whole lines and the position to
//! resume from, so a restarted daemon continues where the stored log ends.
//!
//! `worker_job` reads the same job ID from a worker log copied out of a stopped runner, which no script can run in.

use std::fmt::Write as _;

use crate::store::LogPosition;

/// The script; `bash -c SCRIPT capture <page> <offset>` resumes at that page and byte offset.
pub const SCRIPT: &str = r#"
diag=/home/runner/_diag keep=/home/runner/_diag/.runnerd-pages start_page=$1 start_offset=$2 job= record=
declare -A off
page_of() { local page=${1##*_}; echo "${page%.log}"; }
while :; do
    if [ -z "$job" ]; then
        for w in "$diag"/Worker_*.log; do
            [ -f "$w" ] || continue
            id=$(grep -m1 -A1 '"k": "check_run_id"' "$w" | sed -n 's/.*"v": \([0-9]*\).*/\1/p')
            run=$(grep -m1 -A1 '"k": "run_id"' "$w" | sed -n 's/.*"v": "\([0-9]*\)".*/\1/p')
            record=$(grep -m1 '"jobId": "' "$w" | sed -n 's/.*"jobId": "\([0-9a-f-]*\)".*/\1/p')
            # Only now: creating _diag before the runner does would leave it unwritable to the runner.
            if [ -n "$id" ] && [ -n "$run" ] && [ -n "$record" ]; then
                job=$id
                mkdir -p -m 700 "$keep"
                echo "@@job $id $run"
            fi
        done
    fi
    if [ -n "$job" ]; then
        for f in "$diag"/pages/*_"$record"_*.log; do
            [ -f "$f" ] && [ "$(page_of "$f")" -ge "$start_page" ] && [ ! -e "$keep/${f##*/}" ] \
                && ln "$f" "$keep/${f##*/}" 2> /dev/null
        done
        for k in "$keep"/*_"$record"_*.log; do
            [ -f "$k" ] || continue
            name=${k##*/} page=$(page_of "$k") gone=
            if [ "$page" -lt "$start_page" ]; then rm -f "$k"; continue; fi
            [ -e "$diag/pages/$name" ] || gone=1
            size=$(stat -c %s "$k")
            o=${off[$name]:-0}
            [ -n "${off[$name]}" ] || [ "$page" -ne "$start_page" ] || o=$start_offset
            if [ "$size" -gt "$o" ]; then
                echo "@@page $page $o $((size - o))"
                tail -c +$((o + 1)) "$k" | head -c $((size - o))
            fi
            off[$name]=$size
            if [ -n "$gone" ]; then echo "@@end $page"; rm -f "$k"; fi
        done
    fi
    sleep 0.2
done
"#;

/// The runner's diagnostic directory, which `SCRIPT` reads too.
pub const DIAG: &str = "/home/runner/_diag";

/// How the names of the job's worker logs in `DIAG` start, which `worker_job` reads.
pub const WORKER_LOG: &str = "Worker_";

/// The job (`check_run_id`) a worker log's job message names, read as `SCRIPT` reads it: the first line with the key,
/// or the one after it, holds its value. `None` when the log names no job, as before the runner took one.
pub fn worker_job(log: &str) -> Option<u64> {
    let lines = log
        .lines()
        .skip_while(|line| !line.contains(r#""k": "check_run_id""#));
    lines.take(2).find_map(|line| {
        let (_, value) = line.split_once(r#""v": "#)?;
        let digits = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        value[..digits].parse().ok()
    })
}

/// The `docker exec` command line that captures a job from `position` on.
pub fn command(position: LogPosition) -> Vec<String> {
    vec![
        "bash".to_owned(),
        "-c".to_owned(),
        SCRIPT.to_owned(),
        "capture".to_owned(),
        position.page.to_string(),
        position.offset.to_string(),
    ]
}

/// What the script reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    /// The runner took this job.
    Job {
        /// GitHub's job ID.
        id: u64,
        /// The workflow run.
        run_id: u64,
    },
    /// Whole log lines, and where the capture resumes after them.
    Lines {
        /// The lines, without their line ends.
        lines: Vec<String>,
        /// The position after the last line.
        position: LogPosition,
    },
}

/// Turns the script's output, in chunks of any size, into `Captured`.
#[derive(Debug, Default)]
pub struct Parser {
    input: Vec<u8>,
    /// Bytes of the current `@@page` block still to come.
    remaining: usize,
    /// The page being read, and the offset in it where `partial` starts.
    page: u64,
    partial_offset: u64,
    /// The start of a line whose end has not arrived, at most `MAX_LINE` bytes of it.
    partial: Vec<u8>,
    /// That line's length so far, with the bytes past `MAX_LINE` that were dropped.
    partial_len: u64,
}

/// The longest line kept: the rest of a longer one is dropped as it arrives, so one line never holds more memory.
pub const MAX_LINE: usize = 1 << 20;

/// Bytes of lines per `Captured::Lines`, about: a long page comes in batches, each with where to resume after it.
const BATCH: usize = 1 << 20;

const BOM: &[u8] = "\u{feff}".as_bytes();

fn text(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line.strip_prefix(BOM).unwrap_or(line)).into_owned()
}

impl Parser {
    /// Feeds output and returns what it completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Captured> {
        self.input.extend_from_slice(chunk);
        let mut captured = Vec::new();
        loop {
            if self.remaining > 0 {
                if self.input.is_empty() {
                    break;
                }
                let take = self.remaining.min(self.input.len());
                let data: Vec<u8> = self.input.drain(..take).collect();
                self.remaining -= take;
                self.data(&data, &mut captured);
                continue;
            }
            let Some(end) = self.input.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let header: Vec<u8> = self.input.drain(..=end).collect();
            self.header(&String::from_utf8_lossy(&header[..end]), &mut captured);
        }
        captured
    }

    fn header(&mut self, header: &str, captured: &mut Vec<Captured>) {
        let mut words = header.split(' ');
        let numbers: Vec<u64> = words
            .clone()
            .skip(1)
            .filter_map(|word| word.parse().ok())
            .collect();
        match (words.next(), numbers.as_slice()) {
            (Some("@@job"), [id, run_id]) => captured.push(Captured::Job {
                id: *id,
                run_id: *run_id,
            }),
            (Some("@@page"), [page, offset, length]) => {
                if *page != self.page || *offset != self.partial_offset + self.partial_len {
                    // A new page, or a resumed one: whatever was pending belongs to a page now complete.
                    self.page = *page;
                    self.take_line();
                    self.partial_offset = *offset;
                }
                self.remaining = usize::try_from(*length).unwrap_or(usize::MAX);
            }
            (Some("@@end"), [page]) if *page == self.page && self.partial_len > 0 => {
                let line = self.take_line();
                self.partial_offset = 0;
                captured.push(Captured::Lines {
                    lines: vec![line],
                    position: LogPosition {
                        page: page + 1,
                        offset: 0,
                    },
                });
            }
            (Some("@@end"), [page]) if *page == self.page => {
                self.partial_offset = 0;
                captured.push(Captured::Lines {
                    lines: Vec::new(),
                    position: LogPosition {
                        page: page + 1,
                        offset: 0,
                    },
                });
            }
            // Anything else is not from the script; nothing to do with it.
            _ => {}
        }
    }

    /// The pending line as text, said to be cut if it was; clears it.
    fn take_line(&mut self) -> String {
        let mut line = text(&self.partial);
        let dropped = self.partial_len - self.partial.len() as u64;
        if dropped > 0 {
            let _ = write!(
                line,
                " [runnerd: {dropped} more bytes of this line not kept]"
            );
        }
        self.partial.clear();
        self.partial_len = 0;
        line
    }

    fn data(&mut self, data: &[u8], captured: &mut Vec<Captured>) {
        let mut lines = Vec::new();
        let mut bytes = 0;
        for (index, segment) in data.split(|byte| *byte == b'\n').enumerate() {
            // Every segment but the first follows a newline, which ended the pending line.
            if index > 0 {
                let length = self.partial_len + 1;
                let line = self.take_line();
                self.partial_offset += length;
                bytes += line.len() + 1;
                lines.push(line);
                if bytes >= BATCH {
                    bytes = 0;
                    captured.push(Captured::Lines {
                        lines: std::mem::take(&mut lines),
                        position: self.position(),
                    });
                }
            }
            let room = MAX_LINE - self.partial.len();
            self.partial
                .extend_from_slice(&segment[..room.min(segment.len())]);
            self.partial_len += segment.len() as u64;
        }
        if !lines.is_empty() {
            captured.push(Captured::Lines {
                lines,
                position: self.position(),
            });
        }
    }

    /// Where the capture resumes: after the last complete line.
    fn position(&self) -> LogPosition {
        LogPosition {
            page: self.page,
            offset: self.partial_offset,
        }
    }

    /// Bytes held for the pending line.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.partial.capacity()
    }
}
