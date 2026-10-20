use std::fmt;

/// Every failure the AWS layer can report. `LoginRequired` is separated out so the
/// UI can offer an SSO sign-in instead of a raw error.
#[derive(Debug, Clone)]
pub enum AwsError {
    LoginRequired { profile: String, detail: String },
    Api { status: u16, code: String, message: String },
    Config(String),
    Network(String),
}

pub type Result<T> = std::result::Result<T, AwsError>;

impl AwsError {
    pub fn is_login_required(&self) -> bool {
        matches!(self, AwsError::LoginRequired { .. })
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            AwsError::Api { status, code, .. } => {
                *status == 429
                    || *status >= 500
                    || code.contains("Throttl")
                    || code == "TooManyRequestsException"
                    || code == "RequestLimitExceeded"
            }
            AwsError::Network(_) => true,
            _ => false,
        }
    }

    /// Errors meaning the temporary credentials are no longer accepted.
    pub fn is_expired_credentials(&self) -> bool {
        matches!(self, AwsError::Api { code, .. }
            if code == "ExpiredTokenException"
                || code == "ExpiredToken"
                || code == "UnrecognizedClientException"
                || code == "InvalidClientTokenId")
    }

    /// One-line text for status bars and toasts.
    pub fn short(&self) -> String {
        match self {
            AwsError::LoginRequired { profile, .. } => {
                format!("AWS session for '{profile}' expired — press L to sign in")
            }
            AwsError::Api { code, message, .. } if message.is_empty() => code.clone(),
            AwsError::Api { code, message, .. } => format!("{code}: {message}"),
            AwsError::Config(m) | AwsError::Network(m) => m.clone(),
        }
    }
}

impl fmt::Display for AwsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AwsError::LoginRequired { profile, detail } => write!(
                f,
                "AWS session for profile '{profile}' is missing or expired ({detail}). Run: {} login",
                crate::util::program()
            ),
            AwsError::Api { status, code, message } => write!(f, "{code} ({status}): {message}"),
            AwsError::Config(m) => write!(f, "{m}"),
            AwsError::Network(m) => write!(f, "network error: {m}"),
        }
    }
}

impl std::error::Error for AwsError {}
