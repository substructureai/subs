use async_trait::async_trait;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};

use crate::protocol::{ContentPart, StoredContent};

pub const BLOB_SCHEME: &str = "blob://";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub tenant_id: String,
    pub id: String,
    pub mime: String,
    pub name: Option<String>,
    pub size: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Query {
    mime: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl BlobRef {
    pub fn uri(&self) -> String {
        let query = serde_urlencoded::to_string(Query {
            mime: self.mime.clone(),
            size: self.size,
            name: self.name.clone(),
        })
        .unwrap_or_default();
        format!("{BLOB_SCHEME}{}/{}?{query}", enc(&self.tenant_id), self.id)
    }

    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix(BLOB_SCHEME)?;
        let (path, query) = rest.split_once('?')?;
        let (tenant, id) = path.split_once('/')?;
        uuid::Uuid::try_parse(id).ok()?;
        let q: Query = serde_urlencoded::from_str(query).ok()?;
        Some(Self {
            tenant_id: dec(tenant)?,
            id: id.to_string(),
            mime: q.mime,
            name: q.name,
            size: q.size,
        })
    }
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn dec(s: &str) -> Option<String> {
    percent_decode_str(s).decode_utf8().ok().map(Into::into)
}

#[derive(Debug)]
pub enum BlobError {
    NotFound,
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::NotFound => write!(f, "blob not found"),
            BlobError::Invalid(m) => write!(f, "invalid blob: {m}"),
            BlobError::Io(m) => write!(f, "blob io: {m}"),
        }
    }
}

impl std::error::Error for BlobError {}

pub struct NewBlob {
    pub tenant_id: String,
    pub mime: String,
    pub name: Option<String>,
    pub bytes: Vec<u8>,
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, blob: NewBlob) -> Result<BlobRef, BlobError>;
    async fn get(&self, r: &BlobRef) -> Result<Vec<u8>, BlobError>;

    /// Bytes `range` of the blob, fewer at its end. A blob never changes, so
    /// a read always gives the same bytes. Scripts stream files a range at a
    /// time, so a store should fetch only the range.
    async fn read(&self, r: &BlobRef, range: std::ops::Range<u64>) -> Result<Vec<u8>, BlobError>;
}

/// `range` of `bytes`, clamped to their end.
pub fn slice(bytes: &[u8], range: std::ops::Range<u64>) -> Vec<u8> {
    let end = (range.end as usize).min(bytes.len());
    let start = (range.start as usize).min(end);
    bytes[start..end].to_vec()
}

pub struct MemoryBlobStore(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

impl Default for MemoryBlobStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(&self, blob: NewBlob) -> Result<BlobRef, BlobError> {
        let id = uuid::Uuid::now_v7().to_string();
        let size = blob.bytes.len() as u64;
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), blob.bytes);
        Ok(BlobRef {
            tenant_id: blob.tenant_id,
            id,
            mime: blob.mime,
            name: blob.name,
            size,
        })
    }

    async fn get(&self, r: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&r.id)
            .cloned()
            .ok_or(BlobError::NotFound)
    }

    async fn read(&self, r: &BlobRef, range: std::ops::Range<u64>) -> Result<Vec<u8>, BlobError> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&r.id)
            .map(|bytes| slice(bytes, range))
            .ok_or(BlobError::NotFound)
    }
}

impl From<ContentPart> for StoredContent {
    fn from(part: ContentPart) -> Self {
        let link = |uri: String, name: Option<String>, mime_type: Option<&str>| Self::Link {
            uri,
            name,
            mime_type: mime_type.map(str::to_string),
        };
        match part {
            ContentPart::Text { text } => Self::Text { text },
            ContentPart::ImageUrl { image_url } => link(image_url.url, None, Some("image/*")),
            ContentPart::VideoUrl { video_url } => link(video_url.url, None, Some("video/*")),
            ContentPart::File { file } => link(file.file_data, Some(file.filename), None),
            ContentPart::InputAudio { input_audio } => {
                let mime = format!("audio/{}", input_audio.format);
                let uri = format!("data:{mime};base64,{}", input_audio.data);
                link(uri, None, Some(&mime))
            }
        }
    }
}
