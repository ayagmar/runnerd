//! The store's writer: where the jobs table may be updated.
pub const STATE: &str = "UPDATE jobs SET state = 'completed' WHERE id = ?1";
