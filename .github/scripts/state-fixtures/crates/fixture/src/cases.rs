//! Sealed structs: private fields in a leaf file, written from outside their own methods.

pub struct Taken {
    k: Option<u64>,
    l: Option<u64>,
}

/// A second writer that only takes: not an assignment, and still a write.
pub fn empty(taken: &mut Taken) -> Option<u64> {
    std::mem::take(&mut taken.k)
}

pub struct Borrowed {
    m: Option<u64>,
    n: Vec<u64>,
}

/// Writes through `as_mut` and `iter_mut`.
pub fn bump(borrowed: &mut Borrowed) {
    if let Some(m) = borrowed.m.as_mut() {
        *m += 1;
    }
    for n in borrowed.n.iter_mut() {
        *n += 1;
    }
}

/// Written by a method named like a constructor: it has a `self`, so the value already exists.
pub struct BuildNamed {
    o: Option<u64>,
    p: Option<u64>,
}

pub struct Clearing;

impl Clearing {
    pub fn with_cleared(&mut self, named: &mut BuildNamed) {
        named.o = None;
        named.p = None;
    }
}

/// Filled by a free constructor: nobody else holds it yet, so this is not a second writer.
pub struct BuiltFree {
    q: Option<u64>,
    r: Vec<u64>,
}

pub fn from_parts(built: &mut BuiltFree) {
    built.q = None;
    built.r.clear();
}

/// Deserialized in no way: a doc comment does not make wire data.
pub struct DocSaysWire {
    s: Option<u64>,
    t: Vec<u64>,
}

pub fn fill(doc: &mut DocSaysWire) {
    doc.s = Some(1);
    doc.t.push(1);
}

/// Life-cycle fields behind what only guards them.
pub struct Guarded {
    u: std::sync::Mutex<Vec<u64>>,
    v: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
}

pub fn guard(guarded: &Guarded) {
    guarded.u.lock().unwrap().push(1);
    *guarded.v.lock().unwrap() = None;
}

pub struct Flags {
    w: std::sync::atomic::AtomicBool,
    x: AtomicBool,
}

pub fn raise(flags: &Flags) {
    flags.w.store(true, Ordering::SeqCst);
    flags.x.store(true, Ordering::SeqCst);
}

pub struct FullPaths {
    y: std::collections::BTreeMap<u64, u64>,
    z: std::option::Option<u64>,
}

pub fn path(paths: &mut FullPaths) {
    paths.y.clear();
    paths.z = None;
}

/// Written the way this repository's code is: through `sync::lock`, from outside the type, with no field written bare.
pub struct LockedOpen {
    pub aa: Arc<Mutex<Option<u64>>>,
    pub ab: Mutex<Vec<u64>>,
}

/// Private in a leaf file, and a second owner in the same file writes it through `sync::lock`.
pub struct LockedSealed {
    ac: Mutex<Option<u64>>,
    ad: Mutex<Vec<u64>>,
}

pub fn second_owner(sealed: &LockedSealed) {
    *lock(&sealed.ac) = None;
    lock(&sealed.ad).push(1);
}

/// The other way code here survives a poisoned lock, naming the function and with a closure.
pub struct Poisoned {
    pub ae: Mutex<Option<u64>>,
    pub af: Mutex<Vec<u64>>,
}

pub fn recover(poisoned: &Poisoned) {
    *poisoned.ae.lock().unwrap_or_else(PoisonError::into_inner) = None;
    poisoned.af.lock().unwrap_or_else(|poison| poison.into_inner()).push(1);
}

/// A guard taken only to read writes nothing: its one write is not two.
pub struct LockedRead {
    pub ag: Mutex<Option<u64>>,
    pub ah: Mutex<Vec<u64>>,
}

pub fn read(locked: &LockedRead) -> usize {
    *lock(&locked.ag) = None;
    lock(&locked.ah).len()
}
