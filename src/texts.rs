//! A file's text, read from disk when asked. The server no longer keeps every file in memory
//! (that was most of its memory on a large repo); the OS keeps recently read files cached,
//! outside this process.

use std::path::Path;
use std::sync::Arc;

/// The file's text, from disk.
pub fn get(root: &Path, rel: &str) -> Option<Arc<str>> {
    std::fs::read(root.join(rel)).ok().map(|b| Arc::from(String::from_utf8_lossy(&b).as_ref()))
}
