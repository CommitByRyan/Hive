use std::fmt;

/// Platform-level failure, kept free of policy-specific codes. `hive-policy`
/// maps these onto its stable public error codes.
#[derive(Debug)]
pub enum PlatformError {
    Io {
        context: String,
        source: std::io::Error,
    },
    ReparsePoint(String),
    OutsideRoot(String),
    CrossVolume(String),
    NotFound(String),
    Unsupported(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::ReparsePoint(message) => formatter.write_str(message),
            Self::OutsideRoot(message) => formatter.write_str(message),
            Self::CrossVolume(message) => formatter.write_str(message),
            Self::NotFound(message) => formatter.write_str(message),
            Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl PlatformError {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::ReparsePoint(_) => "reparse_point",
            Self::OutsideRoot(_) => "outside_root",
            Self::CrossVolume(_) => "cross_volume",
            Self::NotFound(_) => "not_found",
            Self::Unsupported(_) => "unsupported",
        }
    }
}
