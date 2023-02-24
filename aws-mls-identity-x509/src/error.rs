use aws_mls_core::identity::CredentialType;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum X509IdentityError {
    #[error("unsupported credential type {0:?}")]
    UnsupportedCredentialType(CredentialType),
    #[error("signing identity public key does not match the leaf certificate")]
    SignatureKeyMismatch,
    #[error("unable to parse certificate chain data")]
    InvalidCertificateChain,
    #[error("invalid offset within certificate chain")]
    InvalidOffset,
    #[error("empty certificate chain")]
    EmptyCertificateChain,
    #[error(transparent)]
    CredentialEncodingError(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    X509ReaderError(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    IdentityExtractorError(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    X509ValidationError(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    IdentityWarningProviderError(Box<dyn std::error::Error + Send + Sync + 'static>),
}
