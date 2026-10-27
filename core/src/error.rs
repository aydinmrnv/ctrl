/// Every error that crosses the FFI boundary.
///
/// The enum is flat, so foreign code sees a variant plus a message. That keeps
/// the Swift and C# bindings simple and stops internal error types from leaking
/// into the public API.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum CtrlError {
    #[error("I/O error: {0}")]
    Io(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("secret store error: {0}")]
    SecretStore(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("the pairing code was wrong")]
    WrongCode,
    #[error("pairing failed: {0}")]
    PairingFailed(String),
    #[error("no pairing is in progress")]
    NotPairing,
    #[error("peer is not trusted")]
    UnknownPeer,
    #[error("device {0} is not reachable")]
    Unreachable(String),
    #[error("payload not found; the sender may have restarted or dropped it")]
    PayloadNotFound,
    #[error("clip is concealed and cannot leave this device")]
    Concealed,
    #[error("cancelled")]
    Cancelled,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("node has shut down")]
    ShutDown,
}

pub type Result<T, E = CtrlError> = std::result::Result<T, E>;

impl From<std::io::Error> for CtrlError {
    fn from(e: std::io::Error) -> Self {
        CtrlError::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for CtrlError {
    fn from(e: rusqlite::Error) -> Self {
        CtrlError::Storage(e.to_string())
    }
}

impl From<postcard::Error> for CtrlError {
    fn from(e: postcard::Error) -> Self {
        CtrlError::Protocol(format!("malformed message: {e}"))
    }
}

impl From<quinn::ConnectionError> for CtrlError {
    fn from(e: quinn::ConnectionError) -> Self {
        CtrlError::Network(e.to_string())
    }
}

impl From<quinn::ConnectError> for CtrlError {
    fn from(e: quinn::ConnectError) -> Self {
        CtrlError::Network(e.to_string())
    }
}

impl From<quinn::WriteError> for CtrlError {
    fn from(e: quinn::WriteError) -> Self {
        CtrlError::Network(e.to_string())
    }
}

impl From<quinn::ReadExactError> for CtrlError {
    fn from(e: quinn::ReadExactError) -> Self {
        CtrlError::Network(e.to_string())
    }
}

impl From<quinn::ReadError> for CtrlError {
    fn from(e: quinn::ReadError) -> Self {
        CtrlError::Network(e.to_string())
    }
}

impl From<quinn::ClosedStream> for CtrlError {
    fn from(e: quinn::ClosedStream) -> Self {
        CtrlError::Network(e.to_string())
    }
}

/// Required by UniFFI so that a foreign `SecretStore` or listener that throws
/// an unexpected exception maps onto our error type instead of panicking.
impl From<uniffi::UnexpectedUniFFICallbackError> for CtrlError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        CtrlError::SecretStore(e.reason)
    }
}
