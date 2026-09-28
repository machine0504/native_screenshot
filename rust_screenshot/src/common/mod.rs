//! Common value types and errors shared across the helper.

pub mod error;
pub mod types;

pub use error::{Result, ScreenshotError};
pub use types::{Rect, Snapshot};
