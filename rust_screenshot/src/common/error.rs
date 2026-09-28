//! Error types for screenshot operations.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ScreenshotError {
    #[error("Overlay window error: {0}")]
    Overlay(String),

    #[error("Screen capture failed: {0}")]
    Capture(String),

    #[error("Image encoding failed: {0}")]
    Encoding(String),

    #[error("Clipboard error: {0}")]
    Clipboard(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ScreenshotError>;

impl From<std::io::Error> for ScreenshotError {
    fn from(err: std::io::Error) -> Self {
        ScreenshotError::Other(err.to_string())
    }
}

impl From<image::ImageError> for ScreenshotError {
    fn from(err: image::ImageError) -> Self {
        ScreenshotError::Encoding(err.to_string())
    }
}
