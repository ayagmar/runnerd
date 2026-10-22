//! The job a runner took and the job's log, read from inside its container while it runs. A small script, run with
//! `docker exec` as root, waits for the runner's `_diag/Worker_*.log`, prints the job's ID (`check_run_id`) and run
//! ID from the job message there, then follows the job's log pages in `_diag/pages`, which the runner writes in
//! 4 KiB blocks and deletes once uploaded: the script hard-links each page so none is lost, and prints what it
//! has not printed before. These pages are what GitHub shows, with secrets already masked by the runner.
//!
//! The script's output: `@@job <id> <run id>`, then `@@page <page> <offset> <length>` followed by exactly that
//! many bytes, and `@@end <page>` once a page is complete. `Parser` turns it into whole lines and the position to
//! resume from, so a restarted daemon continues where the stored log ends.

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
    /// The start of a line whose end has not arrived.
    partial: Vec<u8>,
}

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
                if *page != self.page || *offset != self.partial_offset + self.partial.len() as u64
                {
                    // A new page, or a resumed one: whatever was pending belongs to a page now complete.
                    self.page = *page;
                    self.partial.clear();
                    self.partial_offset = *offset;
                }
                self.remaining = usize::try_from(*length).unwrap_or(usize::MAX);
            }
            (Some("@@end"), [page]) if *page == self.page && !self.partial.is_empty() => {
                let line = text(&std::mem::take(&mut self.partial));
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

    fn data(&mut self, data: &[u8], captured: &mut Vec<Captured>) {
        self.partial.extend_from_slice(data);
        let Some(last) = self.partial.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let rest = self.partial.split_off(last + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        let lines = complete[..last]
            .split(|byte| *byte == b'\n')
            .map(text)
            .collect();
        self.partial_offset += complete.len() as u64;
        captured.push(Captured::Lines {
            lines,
            position: LogPosition {
                page: self.page,
                offset: self.partial_offset,
            },
        });
    }
}
