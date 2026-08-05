use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathDto {
    Text(String),
    Bytes { kind: String, bytes: Vec<u8> },
}
impl From<PathBuf> for PathDto {
    fn from(path: PathBuf) -> Self {
        match path.into_os_string().into_string() {
            Ok(value) => Self::Text(value),
            Err(value) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    Self::Bytes {
                        kind: "bytes".into(),
                        bytes: value.as_os_str().as_bytes().to_vec(),
                    }
                }
                #[cfg(not(unix))]
                {
                    Self::Text(value.to_string_lossy().into_owned())
                }
            }
        }
    }
}
impl From<&Path> for PathDto {
    fn from(path: &Path) -> Self {
        Self::from(path.to_owned())
    }
}
impl PathDto {
    pub fn into_path(self) -> Result<PathBuf, String> {
        match self {
            Self::Text(value) => Ok(PathBuf::from(value)),
            Self::Bytes { kind, bytes } if kind == "bytes" => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
                }
                #[cfg(not(unix))]
                {
                    String::from_utf8(bytes)
                        .map(PathBuf::from)
                        .map_err(|_| "path bytes are not UTF-8".into())
                }
            }
            Self::Bytes { .. } => Err("unknown path encoding".into()),
        }
    }
}

/// A path representation that remains lossless when crossing a JSON boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPath(PathBuf);

impl StoredPath {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
    pub fn into_path(self) -> PathBuf {
        self.0
    }
}

impl From<PathBuf> for StoredPath {
    fn from(value: PathBuf) -> Self {
        Self(value)
    }
}
impl From<StoredPath> for PathBuf {
    fn from(value: StoredPath) -> Self {
        value.0
    }
}
impl Serialize for StoredPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        PathDto::from(self.0.clone()).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoredPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        PathDto::deserialize(deserializer)?
            .into_path()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Clean,
    Dirty,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeClass {
    Primary,
    Linked,
    Bare,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub head_oid: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub classification: WorktreeClass,
    pub status: CheckoutStatus,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub locked: Option<Reason>,
    pub prunable: Option<Reason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListData {
    pub repository: RepositorySummary,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "serialize_optional_path")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListResult {
    pub data: ListData,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositorySummary {
    #[serde(serialize_with = "serialize_path")]
    pub common_dir: PathBuf,
    pub bare: bool,
}

pub fn serialize_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(value) = path.to_str() {
        serializer.serialize_str(value)
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            #[derive(Serialize)]
            struct Bytes<'a> {
                kind: &'static str,
                bytes: &'a [u8],
            }
            Bytes {
                kind: "bytes",
                bytes: path.as_os_str().as_bytes(),
            }
            .serialize(serializer)
        }
        #[cfg(not(unix))]
        {
            serializer.serialize_str(&path.to_string_lossy())
        }
    }
}

pub fn serialize_optional_path<S>(path: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serialize_path(path, serializer),
        None => serializer.serialize_none(),
    }
}

impl Worktree {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            head_oid: None,
            branch: None,
            detached: false,
            classification: WorktreeClass::Unknown,
            status: CheckoutStatus::Unknown,
            upstream: None,
            ahead: None,
            behind: None,
            locked: None,
            prunable: None,
        }
    }
}
