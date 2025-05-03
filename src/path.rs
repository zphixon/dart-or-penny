use crate::{ErrorInner, Error};
use std::{
    fmt::Display, io::ErrorKind, path::{Path, PathBuf}
};

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct LocalPath(pub PathBuf);

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ServePath(PathBuf);

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ThumbnailPath(PathBuf);

impl ThumbnailPath {
    pub fn thumbnail_path(&self) -> &Path {
        &self.0
    }
}

impl LocalPath {
    pub fn local_path(&self) -> &Path {
        &self.0
    }

    pub fn from_serve_path(
        db: &crate::Database,
        config: &crate::Config,
        ServePath(serve_path): &ServePath,
    ) -> Result<LocalPath, Error> {
        if let Some(page_root) = config.page_root.as_ref() {
            let page_root = PathBuf::from(page_root);
            let suffix = serve_path.strip_prefix(&page_root)?;
            let local_path = db.file_dir.local_path().join(suffix);
            let path = local_path.canonicalize().map_err(|err| match err.kind() {
                ErrorKind::NotFound => ErrorInner::FileNotFound(local_path),
                _ => ErrorInner::Io(err),
            })?;
            Ok(LocalPath(path))
        } else {
            Ok(LocalPath(db.file_dir.local_path().join(serve_path)))
        }
    }
}

impl Display for LocalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.local_path().display())
    }
}

impl ServePath {
    pub fn to_string(&self, percent_encode: bool) -> String {
        use std::path::Component;
        let flat = |part| match part {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => None,
            Component::ParentDir => unreachable!("directory traversal"),
            Component::Normal(part) => Some(part),
        };

        let mut encoded = String::from("/");
        let num_parts = self.0.components().flat_map(flat).count();

        for (i, part) in self.0.components().flat_map(flat).enumerate() {
            let part = part.to_string_lossy().to_string();
            if percent_encode {
                encoded += &crate::percent_encode(&part);
            } else {
                encoded += &part;
            }
            if i + 1 != num_parts {
                encoded += "/";
            }
        }

        encoded
    }

    pub fn from_local_path(
        db: &crate::Database,
        config: &crate::Config,
        LocalPath(local_path): &LocalPath,
    ) -> Result<ServePath, Error> {
        let stripped_local_path = local_path.strip_prefix(db.file_dir.local_path())?;
        if let Some(page_root) = config.page_root.as_ref() {
            let page_root = PathBuf::from(page_root);
            Ok(ServePath(page_root.join(stripped_local_path)))
        } else {
            Ok(ServePath(stripped_local_path.to_path_buf()))
        }
    }
}

impl Display for ServePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string(false))
    }
}

impl From<PathBuf> for ServePath {
    fn from(value: PathBuf) -> Self {
        Self(value)
    }
}

impl From<PathBuf> for LocalPath {
    fn from(value: PathBuf) -> Self {
        Self(value)
    }
}

impl From<PathBuf> for ThumbnailPath {
    fn from(value: PathBuf) -> Self {
        Self(value)
    }
}
