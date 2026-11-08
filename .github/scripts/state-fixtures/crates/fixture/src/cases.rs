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
