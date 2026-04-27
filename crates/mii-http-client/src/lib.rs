//! Runtime support for clients generated from `mii-http` specs.

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::fmt;
use std::path::PathBuf;

pub use bytes::Bytes;
pub use mii_http_client_macros::client;
pub use reqwest;
pub use serde;
pub use serde_json;
pub use uuid;

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/');

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    InvalidUrl(String),
    Io(std::io::Error),
    Http(reqwest::Error),
    UnexpectedStatus {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidUrl(message) => write!(f, "invalid URL: {}", message),
            Error::Io(error) => write!(f, "I/O error: {}", error),
            Error::Http(error) => write!(f, "HTTP error: {}", error),
            Error::UnexpectedStatus { status, body } => {
                write!(f, "unexpected HTTP status {}", status)?;
                if !body.is_empty() {
                    write!(f, ": {}", body)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Error::Http(error)
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    base_url: String,
    http: reqwest::Client,
    bearer_token: Option<String>,
}

impl Client {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self> {
        Self::with_http_client(base_url, reqwest::Client::new())
    }

    pub fn with_http_client(base_url: impl AsRef<str>, http: reqwest::Client) -> Result<Self> {
        let base_url = normalize_base_url(base_url.as_ref())?;
        Ok(Self {
            base_url,
            http,
            bearer_token: None,
        })
    }

    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn set_bearer_token(&mut self, token: impl Into<String>) {
        self.bearer_token = Some(token.into());
    }

    pub fn clear_bearer_token(&mut self) {
        self.bearer_token = None;
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer_header: Option<&str>,
    ) -> Result<reqwest::RequestBuilder> {
        let url = self.endpoint_url(path)?;
        let mut request = self.http.request(method, url);
        if let (Some(header), Some(token)) = (bearer_header, self.bearer_token.as_deref()) {
            request = request.header(header, format!("Bearer {}", token));
        }
        Ok(request)
    }

    fn endpoint_url(&self, path: &str) -> Result<reqwest::Url> {
        let path = path.trim_start_matches('/');
        let joined = if path.is_empty() {
            self.base_url.clone()
        } else {
            format!("{}/{}", self.base_url, path)
        };
        reqwest::Url::parse(&joined).map_err(|error| Error::InvalidUrl(error.to_string()))
    }
}

pub struct ByteStream {
    response: reqwest::Response,
}

impl ByteStream {
    pub fn new(response: reqwest::Response) -> Self {
        Self { response }
    }

    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.response.chunk().await.map_err(Error::from)
    }
}

#[derive(Clone, Debug)]
pub struct FilePart {
    source: FilePartSource,
    filename: Option<String>,
    mime: Option<String>,
}

#[derive(Clone, Debug)]
enum FilePartSource {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

impl FilePart {
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self {
            source: FilePartSource::Path(path.into()),
            filename: None,
            mime: None,
        }
    }

    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            source: FilePartSource::Bytes(bytes.into()),
            filename: None,
            mime: None,
        }
    }

    pub fn with_file_name(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }

    pub async fn into_multipart_part(self) -> Result<reqwest::multipart::Part> {
        let mut part = match self.source {
            FilePartSource::Path(path) => reqwest::multipart::Part::file(path).await?,
            FilePartSource::Bytes(bytes) => reqwest::multipart::Part::bytes(bytes),
        };
        if let Some(filename) = self.filename {
            part = part.file_name(filename);
        }
        if let Some(mime) = self.mime {
            part = part.mime_str(&mime)?;
        }
        Ok(part)
    }
}

impl From<Vec<u8>> for FilePart {
    fn from(bytes: Vec<u8>) -> Self {
        FilePart::bytes(bytes)
    }
}

impl From<PathBuf> for FilePart {
    fn from(path: PathBuf) -> Self {
        FilePart::path(path)
    }
}

pub async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(Error::UnexpectedStatus { status, body })
}

pub fn encode_path_segment(value: impl fmt::Display) -> String {
    utf8_percent_encode(&value.to_string(), PATH_SEGMENT_ENCODE_SET).to_string()
}

fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::InvalidUrl("base URL is empty".into()));
    }
    reqwest::Url::parse(trimmed).map_err(|error| Error::InvalidUrl(error.to_string()))?;
    Ok(trimmed.to_string())
}
