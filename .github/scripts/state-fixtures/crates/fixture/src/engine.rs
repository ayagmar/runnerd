//! The root of the check's fixtures. Not built: `check-state.py` reads it.
use std::collections::BTreeMap;
mod children;

pub struct Engine {
    first: BTreeMap<u64, u64>,
    second: BTreeMap<u64, u64>,
    open: Open,
    presence: Presence,
    wrapper: Wrapper,
    taken: crate::cases::Taken,
    borrowed: crate::cases::Borrowed,
    two_bools: TwoBools,
    methods: crate::sealed::Methods,
    wire: Wire,
    marked: Marked,
    occupant: Occupant,
    build_named: crate::cases::BuildNamed,
    built_free: crate::cases::BuiltFree,
    doc_says_wire: crate::cases::DocSaysWire,
    guarded: crate::cases::Guarded,
    flags: crate::cases::Flags,
    full_paths: crate::cases::FullPaths,
}

impl Engine {
    fn pair(&mut self, key: u64) {
        self.first.insert(key, 1);
        self.second.remove(&key);
    }

    fn open(&mut self) {
        self.open.a = Some(1);
        self.open.b = true;
    }

    fn enum_payload(&mut self, state: &mut ThroughEnum, wrapped: &mut ThroughTuple) {
        state.c = Some(1);
        state.d = None;
        wrapped.e = Some(1);
        wrapped.f.push(1);
    }

    fn marked(&mut self, marked: &mut Marked) {
        marked.g = Some(1);
        marked.h = None;
    }
}

fn engaged(occupant: &Occupant) -> bool {
    occupant.busy || occupant.job.is_some()
}

fn occupied(occupant: &Occupant) -> bool {
    occupant.busy || occupant.job.is_some()
}

/// Written from outside, in a file with children.
pub struct Open {
    pub a: Option<u64>,
    pub b: bool,
}

/// Reached only through an enum's payload.
pub enum Presence {
    Listed(ThroughEnum),
    Gone,
}

pub struct ThroughEnum {
    pub c: Option<u64>,
    pub d: Option<u64>,
}

/// Reached only through a tuple struct.
pub struct Wrapper(ThroughTuple);

pub struct ThroughTuple {
    pub e: Option<u64>,
    pub f: Vec<u64>,
}

/// Never written field by field, as a struct replaced whole.
pub struct TwoBools {
    pub short: bool,
    pub stalled: bool,
}

/// Plain wire data, written while it is built.
#[derive(Serialize)]
pub struct Wire {
    pub i: Option<u64>,
    pub j: Option<u64>,
}

/// An exception in `MARKED`.
pub struct Marked {
    pub g: Option<u64>,
    pub h: Option<u64>,
}

/// Its condition is spelled twice above; it has only one field that counts.
pub struct Occupant {
    busy: bool,
    job: u64,
}
