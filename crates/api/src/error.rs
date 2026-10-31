use yoruplay_protocol::ErrorInfo;

use crate::CryptoError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network request failed: {0}")]
    Network(String),
    #[error("request timed out")]
    Timeout,
    #[error("HTTP {status} from {endpoint}")]
    HttpStatus { status: u16, endpoint: String },
    #[error("invalid response from {endpoint}: {message}")]
    Decode { endpoint: String, message: String },
    #[error("NetEase API error {code}: {message}")]
    Api {
        code: i64,
        message: String,
        endpoint: String,
    },
    #[error("login is required")]
    AuthRequired,
    #[error("the NetEase session has expired")]
    SessionExpired,
    #[error("track is not playable: {0}")]
    Unplayable(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("invalid cookie: {0}")]
    InvalidCookie(String),
    #[error("session storage error: {0}")]
    SessionIo(String),
    #[error("session file is corrupt: {0}")]
    SessionCorrupt(String),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Network(_) => "network",
            ApiError::Timeout => "timeout",
            ApiError::HttpStatus { .. } => "http_status",
            ApiError::Decode { .. } => "invalid_response",
            ApiError::Api { .. } => "api_error",
            ApiError::AuthRequired => "not_logged_in",
            ApiError::SessionExpired => "session_expired",
            ApiError::Unplayable(_) => "unplayable",
            ApiError::NotFound(_) => "not_found",
            ApiError::Unsupported(_) => "unsupported",
            ApiError::InvalidArgument(_) => "invalid_argument",
            ApiError::InvalidCookie(_) => "invalid_cookie",
            ApiError::SessionIo(_) => "session_io",
            ApiError::SessionCorrupt(_) => "session_corrupt",
            ApiError::Crypto(_) => "crypto",
        }
    }

    pub fn hint(&self) -> Option<&'static str> {
        match self {
            ApiError::Network(_) | ApiError::Timeout => {
                Some("Check the network connection and try again.")
            }
            ApiError::AuthRequired => Some("Log in with `yoruplay login` or press L in the TUI."),
            ApiError::SessionExpired => Some("Log in again with `yoruplay login`."),
            ApiError::Unplayable(_) => {
                Some("The track may require a subscription, be region restricted, or be removed.")
            }
            ApiError::SessionCorrupt(_) => Some("Log in again; the old auth file can be removed."),
            _ => None,
        }
    }

    /// User-facing message. Kept short; diagnostics stay in tracing logs.
    pub fn user_message(&self) -> String {
        match self {
            ApiError::Network(_) => "Network request failed.".into(),
            ApiError::Timeout => "The NetEase API request timed out.".into(),
            ApiError::HttpStatus { status, .. } => {
                format!("The NetEase server returned HTTP {status}.")
            }
            ApiError::Decode { .. } => "The NetEase server returned an unexpected response.".into(),
            ApiError::Api { code, message, .. } => {
                if message.is_empty() {
                    format!("The NetEase API rejected the request (code {code}).")
                } else {
                    format!("The NetEase API rejected the request ({code}): {message}")
                }
            }
            ApiError::AuthRequired => "Log in to use this feature.".into(),
            ApiError::SessionExpired => "Your NetEase session has expired.".into(),
            ApiError::Unplayable(reason) => format!("This track is not playable: {reason}"),
            ApiError::NotFound(what) => format!("Not found: {what}"),
            ApiError::Unsupported(what) => format!("Not available: {what}"),
            ApiError::InvalidArgument(what) => format!("Invalid request: {what}"),
            ApiError::InvalidCookie(what) => format!("Invalid cookie: {what}"),
            ApiError::SessionIo(what) => format!("Could not use the session file: {what}"),
            ApiError::SessionCorrupt(what) => format!("The saved session is invalid: {what}"),
            ApiError::Crypto(_) => "Could not encrypt the API request.".into(),
        }
    }

    pub fn into_error_info(self) -> ErrorInfo {
        let code = self.code();
        let hint = self.hint().map(str::to_string);
        ErrorInfo {
            code: code.into(),
            message: self.user_message(),
            hint,
        }
    }
}
