//! The two generated identifiers that reach a client and an assertion.
//!
//! Scratch-file suffixes are deliberately absent: they never leave the
//! adapter that makes them, and porting them is how a boundary starts to
//! look like ceremony.

pub trait Ids: Send + Sync {
    fn token_id(&self) -> String;
    fn upload_id(&self) -> String;
}
