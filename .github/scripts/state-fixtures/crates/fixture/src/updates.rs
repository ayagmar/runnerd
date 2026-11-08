//! SQL on the jobs table outside the store's writer.
pub const STATE: &str = "UPDATE jobs SET state = 'lost' WHERE id = ?1";
