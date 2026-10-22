//! Which runner the Docker Hub mirror served, and how many bytes: the mirror's access log (Docker log lines with
//! timestamps, in the combined log format) attributed by source address. A runner container's Docker Engine pulls
//! for every container of its job, so the mirror sees the runner container's address. An address belongs to a
//! runner only between its container's start and its removal, so a later container given the same address never
//! gets an earlier one's bytes.

/// One response the mirror logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// When Docker received the log line, in milliseconds since the epoch.
    pub at: u64,
    /// The client's address.
    pub address: String,
    /// Bytes in the response body.
    pub bytes: u64,
}

/// Parses a line of `docker logs --timestamps` of the mirror: `<RFC 3339> <address> - - [<time>] "<request>"
/// <status> <bytes> …`. Lines of another format (the registry's own messages) are `None`.
pub fn parse(line: &str) -> Option<Served> {
    let (timestamp, line) = line.split_once(' ')?;
    let at = crate::time::parse_ms(timestamp)?;
    let (address, rest) = line.split_once(" - - [")?;
    let (_, rest) = rest.split_once("] \"")?;
    let (_request, rest) = rest.split_once("\" ")?;
    let mut fields = rest.split(' ');
    let _status: u16 = fields.next()?.parse().ok()?;
    let bytes = match fields.next()? {
        "-" => 0,
        bytes => bytes.parse().ok()?,
    };
    if address.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    Some(Served {
        at,
        address: address.to_owned(),
        bytes,
    })
}

/// A runner's hold on an address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    address: String,
    runner: String,
    from: u64,
    until: Option<u64>,
}

/// Which runner held which address when.
#[derive(Debug, Default)]
pub struct Leases {
    leases: Vec<Lease>,
}

impl Leases {
    /// A runner's container got `address` at `from` (ms).
    pub fn start(&mut self, address: &str, runner: &str, from: u64) {
        self.leases.push(Lease {
            address: address.to_owned(),
            runner: runner.to_owned(),
            from,
            until: None,
        });
    }

    /// A runner's container was removed at `until` (ms).
    pub fn end(&mut self, runner: &str, until: u64) {
        for lease in &mut self.leases {
            if lease.runner == runner && lease.until.is_none() {
                lease.until = Some(until);
            }
        }
    }

    /// Forgets leases that ended before `before` (ms): the log was read past them.
    pub fn forget_before(&mut self, before: u64) {
        self.leases
            .retain(|lease| lease.until.is_none_or(|until| until >= before));
    }

    /// The runner that held the address at `at` (ms).
    pub fn holder(&self, address: &str, at: u64) -> Option<&str> {
        self.leases
            .iter()
            .find(|lease| {
                lease.address == address
                    && lease.from <= at
                    && lease.until.is_none_or(|until| at < until)
            })
            .map(|lease| lease.runner.as_str())
    }

    /// Bytes per runner in log lines after `after` (ms), and the timestamp of the last line read.
    pub fn attribute(&self, log: &str, after: u64) -> (Vec<(String, u64)>, u64) {
        let mut totals: Vec<(String, u64)> = Vec::new();
        let mut until = after;
        for served in log
            .lines()
            .filter_map(parse)
            .filter(|served| served.at > after)
        {
            until = until.max(served.at);
            let Some(runner) = self.holder(&served.address, served.at) else {
                continue;
            };
            match totals.iter_mut().find(|(name, _)| name == runner) {
                Some((_, total)) => *total += served.bytes,
                None => totals.push((runner.to_owned(), served.bytes)),
            }
        }
        (totals, until)
    }
}
