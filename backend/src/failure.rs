use std::borrow::Cow;

/// Why a feed could not be fetched or parsed. Every kind has its own stable
/// code, stored in `feeds.last_error_code` and returned by the API, so clients
/// can tell causes apart without matching message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    InvalidUrl,
    BlockedDestination,
    BlockedPort,
    DnsFailed,
    ConnectTimeout,
    ConnectFailed,
    TlsFailed,
    ResponseTimeout,
    DeadlineExceeded,
    TooManyRedirects,
    InvalidRedirect,
    InsecureRedirect,
    HttpStatus,
    UnexpectedNotModified,
    UnsupportedEncoding,
    DecompressFailed,
    BodyReadFailed,
    TooLarge,
    NotAFeed,
    UnsupportedXml,
    XmlTooDeep,
    XmlTooComplex,
    ParseFailed,
    InvalidOpml,
    TooManyFeeds,
    Internal,
}

impl FailureKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::BlockedDestination => "blocked_destination",
            Self::BlockedPort => "blocked_port",
            Self::DnsFailed => "dns_failed",
            Self::ConnectTimeout => "connect_timeout",
            Self::ConnectFailed => "connect_failed",
            Self::TlsFailed => "tls_failed",
            Self::ResponseTimeout => "response_timeout",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::TooManyRedirects => "too_many_redirects",
            Self::InvalidRedirect => "invalid_redirect",
            Self::InsecureRedirect => "insecure_redirect",
            Self::HttpStatus => "http_status",
            Self::UnexpectedNotModified => "unexpected_not_modified",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::DecompressFailed => "decompress_failed",
            Self::BodyReadFailed => "body_read_failed",
            Self::TooLarge => "too_large",
            Self::NotAFeed => "not_a_feed",
            Self::UnsupportedXml => "unsupported_xml",
            Self::XmlTooDeep => "xml_too_deep",
            Self::XmlTooComplex => "xml_too_complex",
            Self::ParseFailed => "parse_failed",
            Self::InvalidOpml => "invalid_opml",
            Self::TooManyFeeds => "too_many_feeds",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: Cow<'static, str>,
    pub retry_after_seconds: Option<i64>,
}

impl Failure {
    pub fn new(kind: FailureKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}
