//! The job a runner took and the job's log, read from inside its container while it runs. A small script, run with
//! `docker exec` as root, waits for the runner's `_diag/Worker_*.log`, prints the job's ID (`check_run_id`) and run
//! ID from the job message there, then follows the job's log pages in `_diag/pages`, which the runner writes in
//! 4 KiB blocks and deletes once uploaded: the script hard-links each page so none is lost, and prints what it
//! has not printed before. These pages are what GitHub shows, with secrets already masked by the runner.
//!
//! The script's output: `@@job <id> <run id>`, then `@@page <page> <offset> <length>` followed by exactly that
//! many bytes, and `@@end <page>` once a page is complete: the runner removed it, or began a later one, as it closes a
//! page before it opens the next. It reads pages by number, never an earlier one after a later. `Parser` turns its
//! output into whole lines and the position to resume from, so a restarted daemon continues where the stored log
//! ends, and into a `Captured::Gap` where bytes are missing.
//!
//! The script's input (ADR 0176): a line `<page>` once the history committed every page before that one ([`ack`]).
//! Only then does the script drop its links of those pages, so a capture that starts again from the stored position
//! finds them. It links a page only while the pages it keeps, those of earlier runs too, hold less than its third
//! argument's bytes, so what it keeps exceeds that by at most what the runner writes to the last page linked; a page
//! it could not link and the runner removed is a gap. When its input ends, the daemon's end is gone, and the script
//! exits.
//!
//! `worker_job` and `worker_run` read the same IDs from a worker log copied out of a stopped runner, which no script can
//! run in.

use std::fmt::Write as _;

use crate::store::LogPosition;

/// The script; `bash -c SCRIPT capture <page> <offset> <keep>` resumes at that page and byte offset, keeping at most
/// `keep` bytes of removed pages the history has not committed.
pub const SCRIPT: &str = r#"
diag=/home/runner/_diag keep=/home/runner/_diag/.runnerd-pages start_page=$1 start_offset=$2 keep_max=$3 job= record=
# The latest page printed: no earlier one is linked after it.
last=0
# Each kept page's bytes printed, and whether its end was.
declare -A off ended
# The page number in a page's file name, in `page`: no command substitution, which forks a subshell.
page_of() { page=${1##*_} page=${page%.log}; }
# The history committed every page before `$1`: their links go on the next pass, and none is linked again.
acked() {
    if [ "$1" -gt "$start_page" ] 2> /dev/null; then start_page=$1 start_offset=0; fi
}
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
        # By page number, as bash lists an indexed array's keys: page 10 after page 9, as the runner writes them.
        # `top` is the latest page there is: the runner closes a page before it opens the next.
        src=() kept=() sizes=() gone=() top=0 held=0
        for f in "$diag"/pages/*_"$record"_*.log; do
            [ -f "$f" ] || continue
            page_of "$f"
            src[page]=$f
            [ "$page" -gt "$top" ] && top=$page
        done
        # What is kept first, every link of earlier runs too, so the bound holds however often a capture starts again.
        for k in "$keep"/*_"$record"_*.log; do
            [ -f "$k" ] || continue
            name=${k##*/}
            page_of "$k"
            if [ "$page" -lt "$start_page" ]; then
                rm -f "$k"
                unset "off[$name]" "ended[$name]"
                continue
            fi
            kept[page]=$k
            [ "$page" -gt "$top" ] && top=$page
            if [ -n "${ended[$name]}" ]; then
                held=$((held + off[$name]))
                continue
            fi
            # Gone before its size is read: bytes the runner wrote before removing it are all there.
            [ -e "$diag/pages/$name" ] || gone[page]=1
            sizes[page]=$(stat -c %s "$k")
            held=$((held + sizes[page]))
        done
        # A new link takes its size of the bound, which it may outgrow by what the runner writes to it after.
        for page in "${!src[@]}"; do
            [ "$page" -ge "$start_page" ] && [ "$page" -ge "$last" ] && [ -z "${kept[page]}" ] || continue
            [ "$held" -lt "$keep_max" ] || break
            f=${src[page]}
            ln "$f" "$keep/${f##*/}" 2> /dev/null || break
            kept[page]=$keep/${f##*/} sizes[page]=$(stat -c %s "$keep/${f##*/}")
            held=$((held + sizes[page]))
        done
        for page in "${!kept[@]}"; do
            k=${kept[page]} name=${kept[page]##*/}
            [ -z "${ended[$name]}" ] || continue
            size=${sizes[page]}
            o=${off[$name]:-0}
            [ -n "${off[$name]}" ] || [ "$page" -ne "$start_page" ] || o=$start_offset
            if [ "$size" -gt "$o" ]; then
                echo "@@page $page $o $((size - o))"
                tail -c +$((o + 1)) "$k" | head -c $((size - o))
            fi
            off[$name]=$size last=$page
            # Whole once the runner removed it or began a later page; kept until the history commits it.
            if [ -n "${gone[page]}" ] || [ "$page" -lt "$top" ]; then
                echo "@@end $page"
                ended[$name]=1
            fi
        done
    fi
    # Waits for the next pass on stdin, where acks come: `read`'s timeout is a builtin, where `sleep` would fork a
    # process five times a second in every running container. A `read` that fails rather than times out (status 128
    # or less) read the end of stdin: the daemon's end is gone, and nothing reads what this prints.
    if read -rt 0.2 line; then
        acked "$line"
        while read -rt 0 && read -r line; do acked "$line"; done
    elif [ $? -le 128 ]; then
        exit 0
    fi
done
"#;

/// The runner's diagnostic directory, which `SCRIPT` reads too.
pub const DIAG: &str = "/home/runner/_diag";

/// How the names of the job's worker logs in `DIAG` start, which `worker_job` and `worker_run` read.
pub const WORKER_LOG: &str = "Worker_";

/// The job (`check_run_id`) a worker log's job message names, read as `SCRIPT` reads it: the first line with the key,
/// or the one after it, holds its value. `None` when the log names no job, as before the runner took one.
pub fn worker_job(log: &str) -> Option<u64> {
    worker_value(log, "check_run_id")
}

/// The workflow run a worker log's job message names, read as `worker_job` reads the job.
pub fn worker_run(log: &str) -> Option<u64> {
    worker_value(log, "run_id")
}

/// The number under `key` in a worker log's job message, quoted or not.
fn worker_value(log: &str, key: &str) -> Option<u64> {
    let named = format!(r#""k": "{key}""#);
    let lines = log.lines().skip_while(|line| !line.contains(&named));
    lines.take(2).find_map(|line| {
        let (_, value) = line.split_once(r#""v": "#)?;
        let value = value.trim_start_matches('"');
        let digits = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        value[..digits].parse().ok()
    })
}

/// The `docker exec` command line that captures a job from `position` on, keeping at most `keep` bytes of removed
/// pages the history has not committed.
pub fn command(position: LogPosition, keep: u64) -> Vec<String> {
    vec![
        "bash".to_owned(),
        "-c".to_owned(),
        SCRIPT.to_owned(),
        "capture".to_owned(),
        position.page.to_string(),
        position.offset.to_string(),
        keep.to_string(),
    ]
}

/// What the script reads once the history committed every page before `page`: it drops its links of them.
pub fn ack(page: u64) -> String {
    format!("{page}\n")
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
    /// Bytes of the log are missing before `position`: the runner removed them before the script kept them, as when
    /// no capture ran for a while, or the script kept as much as it may.
    Gap {
        /// The line the gap cut short, as far as it came.
        cut: Option<String>,
        /// Where the bytes after the gap start, which the capture resumes from.
        position: LogPosition,
    },
}

/// Turns the script's output, in chunks of any size, into `Captured`.
#[derive(Debug)]
pub struct Parser {
    input: Vec<u8>,
    /// Bytes of the current `@@page` block still to come.
    remaining: usize,
    /// The page being read, and the offset in it where `partial` starts: after an `@@end`, the next page at 0.
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
    /// A parser of the output of a script started at `start`.
    pub fn at(start: LogPosition) -> Parser {
        Parser {
            input: Vec::new(),
            remaining: 0,
            page: start.page,
            partial_offset: start.offset,
            partial: Vec::new(),
            partial_len: 0,
        }
    }

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
                let at = LogPosition {
                    page: *page,
                    offset: *offset,
                };
                let next = LogPosition {
                    page: self.page,
                    offset: self.partial_offset + self.partial_len,
                };
                if at != next {
                    if at > next {
                        // Bytes from `next` on are gone: the line they cut ends here, and the gap is named.
                        let cut = (self.partial_len > 0).then(|| self.take_line());
                        captured.push(Captured::Gap { cut, position: at });
                    } else {
                        // Read again from earlier: what was pending is read again too.
                        self.take_line();
                    }
                    self.page = *page;
                    self.partial_offset = *offset;
                }
                // A block that goes on where the last one ended goes on with its pending line, from where it started.
                self.remaining = usize::try_from(*length).unwrap_or(usize::MAX);
            }
            (Some("@@end"), [page]) if *page == self.page => {
                let lines = if self.partial_len > 0 {
                    vec![self.take_line()]
                } else {
                    Vec::new()
                };
                self.page = page + 1;
                self.partial_offset = 0;
                captured.push(Captured::Lines {
                    lines,
                    position: self.position(),
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
