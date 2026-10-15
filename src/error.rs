/// Errors returned by [`crate::run`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No compiled-in display backend could be initialized.
    #[error("no usable display backend: {0}")]
    NoBackend(String),

    /// The requested backend was not compiled in (missing cargo feature) or is not available on
    /// this operating system.
    #[error("backend {0:?} is not available in this build")]
    BackendUnavailable(crate::Backend),

    /// Invalid configuration (options or `EGUI_EMBED_*` environment variables).
    #[error("invalid configuration: {0}")]
    Config(String),

    /// DRM/KMS or GBM failure.
    #[error("KMS: {0}")]
    Kms(String),

    /// Linux framebuffer (fbdev) failure.
    #[error("framebuffer: {0}")]
    Framebuffer(String),

    /// EGL failure.
    #[error("EGL: {0}")]
    Egl(String),

    /// OpenGL (ES) failure.
    #[error("OpenGL: {0}")]
    Gl(String),

    /// I/O failure (polling, signal setup, ...).
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// The app creator closure returned an error.
    #[error("failed to create app: {0}")]
    AppCreation(Box<dyn std::error::Error + Send + Sync>),
}
