use image::{buffer::ConvertBuffer, ImageBuffer, ImageReader, Rgb};
use rouille::Response;
use serde::Serialize;
use std::{
    collections::HashMap,
    error::Error as StdError,
    fmt::Display,
    fs::File as FsFile,
    ops::Deref,
    path::{Path, PathBuf},
};
use tera::{Context as TeraContext, Tera};
use thiserror::Error;

mod path;

use path::{LocalPath, ServePath, ThumbnailPath};

const PAGE_TEMPLATE: &str = include_str!("./page.html.tera");

#[derive(Error, Debug)]
enum ErrorInner {
    #[error("Could not strip prefix: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("File not found: {display}", display = .0.display())]
    FileNotFound(PathBuf),
    #[error("Thumbnail dir must be dir")]
    ThumbnailDirNotDir,
    #[error("File dir must be dir")]
    FileDirNotDir,
    #[error("Cannot serve from system root directory")]
    CannotServeFromRoot,
    #[error("Image conversion error: {0}")]
    Image(#[from] image::error::ImageError),
    #[error("Config error: {0:?}")]
    Config(&'static str),
    #[error("Not a number")]
    NumberParse(#[from] std::num::TryFromIntError),
    #[error("Invalid TOML: {0}")]
    FromToml(#[from] toml::de::Error),
}

#[derive(Debug)]
enum Error {
    Root(ErrorInner),
    Context { context: String, inner: Box<Error> },
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        let inner: &ErrorInner = &*self;
        Some(inner)
    }
}

trait Context<T> {
    fn context<C>(self, context: C) -> Result<T, Error>
    where
        C: std::fmt::Display + Send + Sync + 'static;

    fn with_context<F, C>(self, context_fn: F) -> Result<T, Error>
    where
        F: FnOnce() -> C,
        C: std::fmt::Display + Send + Sync + 'static;
}

impl<T, E: Into<Error>> Context<T> for Result<T, E> {
    fn context<C>(self, context: C) -> Result<T, Error>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        match self {
            Ok(t) => Ok(t),
            Err(err) => Err(Error::Context {
                context: context.to_string(),
                inner: Box::new(err.into()),
            }),
        }
    }

    fn with_context<F, C>(self, context_fn: F) -> Result<T, Error>
    where
        F: FnOnce() -> C,
        C: std::fmt::Display + Send + Sync + 'static,
    {
        match self {
            Ok(t) => Ok(t),
            Err(err) => Err(Error::Context {
                context: context_fn().to_string(),
                inner: Box::new(err.into()),
            }),
        }
    }
}

impl Deref for Error {
    type Target = ErrorInner;

    fn deref(&self) -> &Self::Target {
        match self {
            Error::Root(inner) => inner,
            Error::Context { inner, .. } => {
                let box_ref = Box::as_ref(inner);
                <Error as Deref>::deref(box_ref)
            }
        }
    }
}

impl<T: Into<ErrorInner>> From<T> for Error {
    fn from(value: T) -> Self {
        Error::Root(value.into())
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Root(inner) => write!(f, "{}", inner),
            Error::Context { context, inner } => {
                Display::fmt(inner, f)?;
                write!(f, "\n  {}", context)
            }
        }
    }
}

impl Error {
    fn inner(self) -> ErrorInner {
        match self {
            Error::Root(api_error_inner) => api_error_inner,
            Error::Context { inner, .. } => inner.inner(),
        }
    }

    fn inner_ref(&self) -> &ErrorInner {
        match self {
            Error::Root(error_inner) => error_inner,
            Error::Context { inner, .. } => inner.inner_ref(),
        }
    }
}

fn percent_encode(s: &str) -> String {
    let encoded = rouille::percent_encoding::percent_encode(
        s.as_bytes(),
        rouille::percent_encoding::NON_ALPHANUMERIC,
    );
    encoded.to_string()
}

fn not_found() -> Response {
    Response::text("Not found").with_status_code(404)
}

fn bad_request(why: &str) -> Response {
    Response::text(why).with_status_code(400)
}

fn unauthorized(why: &str) -> Response {
    Response::text(why).with_status_code(401)
}

fn internal_error() -> Response {
    Response::text("Internal server error").with_status_code(500)
}

fn thumbnail_path(of: &Path, thumbnail_dir: &LocalPath) -> ThumbnailPath {
    let name = format!("{}", of.display());

    let mut hasher = md5_rs::Context::new();
    hasher.read(name.as_bytes());
    let hash = hasher
        .finish()
        .into_iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>();

    ThumbnailPath::from(thumbnail_dir.local_path().join(hash).with_extension("jpg"))
}

#[derive(Debug)]
enum File {
    Dir(LocalPath, Vec<File>),
    File(LocalPath),
}

impl File {
    const THUMBNAILABLE_EXTENSIONS: &'static [&'static str] =
        &["png", "tiff", "bmp", "gif", "jpeg", "jpg", "tif"];

    fn walk_dir(
        dir: &LocalPath,
        include_path: &impl Fn(&Path) -> bool,
    ) -> Result<Vec<File>, Error> {
        let mut contents = Vec::new();
        for entry in dir
            .local_path()
            .read_dir()
            .with_context(|| format!("Reading entries in {}", dir))?
        {
            let entry = entry.with_context(|| format!("Reading dir entry in {}", dir))?;
            let path = entry
                .path()
                .canonicalize()
                .with_context(|| format!("Canonicalizing {} in {}", entry.path().display(), dir))?;

            if include_path(&path) {
                contents.push(if path.is_dir() {
                    let local_path = LocalPath::from(path);
                    let inner = Self::walk_dir(&local_path, include_path)
                        .with_context(|| format!("Walking entries in {}", local_path))?;
                    File::Dir(local_path, inner)
                } else {
                    File::File(LocalPath::from(path))
                });
            }
        }

        Ok(contents)
    }

    fn may_be_thumbnailed(&self) -> bool {
        match self {
            File::Dir(..) => false,
            File::File(file) => {
                let Some(ext) = file.local_path().extension() else {
                    return false;
                };

                let ext = ext.to_string_lossy().to_lowercase();
                Self::THUMBNAILABLE_EXTENSIONS.contains(&ext.as_str())
            }
        }
    }

    fn find(&self, local_path: &LocalPath) -> Option<&File> {
        match self {
            File::Dir(my_local_path, vec) => {
                if my_local_path == local_path {
                    Some(self)
                } else {
                    for file in vec.iter() {
                        if let Some(found) = file.find(local_path) {
                            return Some(found);
                        }
                    }
                    None
                }
            }
            File::File(my_local_path) => {
                if my_local_path == local_path {
                    Some(self)
                } else {
                    None
                }
            }
        }
    }

    fn local_path(&self) -> &LocalPath {
        match self {
            File::Dir(local_path, _) => local_path,
            File::File(local_path) => local_path,
        }
    }
}

fn build_thumbnail_db(
    files: &[File],
    thumbnail_dir: &LocalPath,
) -> Result<HashMap<LocalPath, ThumbnailPath>, Error> {
    fn btdb_rec(
        db: &mut HashMap<LocalPath, ThumbnailPath>,
        files: &[File],
        thumbnail_dir: &LocalPath,
    ) -> Result<(), Error> {
        for file in files {
            match file {
                File::Dir(_, files) => btdb_rec(db, files, thumbnail_dir)
                    .with_context(|| format!("Building thumbnail DB in {}", thumbnail_dir))?,
                file @ File::File(path) if file.may_be_thumbnailed() => {
                    let path = path
                        .local_path()
                        .canonicalize()
                        .with_context(|| format!("Canonicalizing thumbnail path at {}", path))?;
                    let thumbnail_path = thumbnail_path(&path, thumbnail_dir);
                    db.insert(LocalPath::from(path), thumbnail_path);
                }
                File::File(path) => {
                    tracing::debug!("skipping thumbnail for {}", path.local_path().display());
                }
            }
        }

        Ok(())
    }

    let mut db = HashMap::new();
    btdb_rec(&mut db, files, thumbnail_dir)
        .with_context(|| format!("Building outer thumbnail DB in {}", thumbnail_dir))?;
    Ok(db)
}

#[derive(Debug)]
pub struct Database {
    file_dir: LocalPath,
    #[allow(dead_code)]
    files: Vec<File>,
    thumbnail_dir: LocalPath,
    thumbnails: HashMap<LocalPath, ThumbnailPath>,
}

impl Database {
    fn open_thumbnail(&self, thumb: &str) -> Result<FsFile, Error> {
        let thumbnail_path = self.thumbnail_dir.local_path().join(thumb);
        Ok(FsFile::open(thumbnail_path).with_context(|| format!("Reading thumbnail {}", thumb))?)
    }

    fn get_context_for(
        &self,
        config: &Config,
        serve_dir: &ServePath,
    ) -> Result<TeraContext, Error> {
        let local_dir = LocalPath::from_serve_path(&self, config, serve_dir)
            .with_context(|| format!("Converting serve path to local path: {:?}", serve_dir))?;

        let mut context = TeraContext::new();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in local_dir
            .local_path()
            .read_dir()
            .with_context(|| format!("Reading dir entry building context in {}", local_dir))?
        {
            let entry = entry.with_context(|| format!("Reading dir entry in {}", local_dir))?;
            let path = LocalPath::from(entry.path());

            if path == self.thumbnail_dir {
                continue;
            }

            let basename = if let Some(basename) = path
                .local_path()
                .file_name()
                .map(|osstr| osstr.to_string_lossy().to_string())
            {
                basename
            } else {
                String::from("<unknown>")
            };

            if path.local_path().is_dir() {
                dirs.push((path, basename));
            } else {
                files.push((path, basename));
            }
        }

        dirs.sort_by(|(_, name1), (_, name2)| name1.cmp(name2));
        files.sort_by(|(_, name1), (_, name2)| name1.cmp(name2));

        fn timestamp(time: std::time::SystemTime) -> String {
            use chrono::{Datelike, Timelike};
            let time: chrono::DateTime<chrono::Local> = time.into();
            let (is_pm, hour) = time.hour12();
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} {}",
                time.year(),
                time.month(),
                time.day(),
                hour,
                time.minute(),
                if is_pm { "PM" } else { "AM" },
            )
        }

        #[derive(Serialize)]
        struct PageItem {
            basename: String,
            filename: String,
            created: String,
            modified: String,
            accessed: String,
            thumbnail_path: Option<String>,
        }

        let mut serde_dirs = Vec::new();
        for (path, basename) in dirs.into_iter() {
            let meta = path.local_path().metadata();
            let (created, modified, accessed) = meta
                .map(|meta| {
                    (
                        meta.created().map(timestamp).unwrap_or_default(),
                        meta.modified().map(timestamp).unwrap_or_default(),
                        meta.accessed().map(timestamp).unwrap_or_default(),
                    )
                })
                .unwrap_or_default();

            serde_dirs.push(PageItem {
                basename,
                created,
                modified,
                accessed,
                filename: ServePath::from_local_path(&self, config, &path)
                    .with_context(|| {
                        format!(
                            "Building serve path for dir {} while creating context in {}",
                            path, local_dir
                        )
                    })?
                    .to_string(true),
                thumbnail_path: None,
            });
        }

        let mut serde_files = Vec::new();
        for (path, basename) in files.into_iter() {
            let meta = path.local_path().metadata();
            let (created, modified, accessed) = meta
                .map(|meta| {
                    (
                        meta.created().map(timestamp).unwrap_or_default(),
                        meta.modified().map(timestamp).unwrap_or_default(),
                        meta.accessed().map(timestamp).unwrap_or_default(),
                    )
                })
                .unwrap_or_default();

            serde_files.push(PageItem {
                basename,
                created,
                modified,
                accessed,
                filename: ServePath::from_local_path(&self, config, &path)
                    .with_context(|| {
                        format!(
                            "Building serve path for file {} while creating context in {}",
                            path, local_dir
                        )
                    })?
                    .to_string(true),
                thumbnail_path: self
                    .thumbnails
                    .get(&path)
                    .into_iter()
                    .flat_map(|thumb| thumb.thumbnail_path().file_name())
                    .map(|thumb| thumb.to_string_lossy().into_owned())
                    .next(),
            });
        }

        context.insert("dirs", &serde_dirs);
        context.insert("files", &serde_files);
        Ok(context)
    }

    fn read_config_and_make_dirs(config: &Config) -> Result<Database, Error> {
        let thumbnail_dir = PathBuf::from(&config.thumbnail_dir);
        let thumbnail_dir = LocalPath::from(thumbnail_dir.canonicalize().with_context(|| {
            format!("Canonicalizing thumbnail dir {}", thumbnail_dir.display())
        })?);
        if !thumbnail_dir.local_path().exists() {
            std::fs::create_dir(thumbnail_dir.local_path()).context("Creating thumbnail dir")?;
        } else if !thumbnail_dir.local_path().is_dir() {
            return Err(ErrorInner::ThumbnailDirNotDir.into());
        }

        let file_dir = PathBuf::from(&config.file_dir);
        let file_dir = LocalPath::from(file_dir.canonicalize().context("Canonicalizing file dir")?);
        if !file_dir.local_path().exists() {
            std::fs::create_dir(file_dir.local_path()).context("Creating file dir")?;
        } else if !file_dir.local_path().is_dir() {
            return Err(ErrorInner::FileDirNotDir.into());
        }
        if file_dir.local_path().parent().is_none() {
            return Err(ErrorInner::CannotServeFromRoot.into());
        }

        let files = File::walk_dir(&file_dir, &|path| path != thumbnail_dir.local_path())
            .context("Walking files in file dir")?;
        let thumbnails =
            build_thumbnail_db(&files, &thumbnail_dir).context("Building thumbnail DB")?;
        Ok(Database {
            file_dir,
            files,
            thumbnail_dir,
            thumbnails,
        })
    }

    fn index_and_build_thumbnail_db(&self, config: &Config) -> Result<(), Error> {
        for (file_path, thumbnail_path) in self.thumbnails.iter() {
            if !thumbnail_path.thumbnail_path().exists() || config.rebuild_thumbnails {
                tracing::info!(
                    "making thumbnail for {} -> {}",
                    file_path.local_path().display(),
                    thumbnail_path.thumbnail_path().display()
                );

                let image = match ImageReader::open(file_path.local_path())
                    .with_context(|| format!("Reading image {}", file_path))?
                    .with_guessed_format()
                    .with_context(|| format!("Guessing format of {}", file_path))?
                    .decode()
                {
                    Ok(image) => image,
                    Err(err) => {
                        tracing::warn!("couldn't make thumbnail: {}", err);
                        continue;
                    }
                };

                let nw = config.thumbnail_size;
                let nh = (config.thumbnail_size as f32
                    * (image.height() as f32 / image.width() as f32))
                    as u32;

                tracing::debug!("resizing to {}x{}", nw, nh);
                let thumbnail = image::imageops::thumbnail(&image, nw, nh);

                let converted: ImageBuffer<Rgb<u8>, _> = thumbnail.convert();
                converted
                    .save(thumbnail_path.thumbnail_path())
                    .with_context(|| {
                        format!(
                            "Saving thumbnail {}",
                            thumbnail_path.thumbnail_path().display()
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn file_list_in(&self, config: &Config, path: &LocalPath) -> Vec<String> {
        let mut file_path = None;
        if path == &self.file_dir {
            file_path = Some(path);
        } else {
            for file in self.files.iter() {
                if let Some(found) = file.find(path) {
                    file_path = Some(found.local_path());
                    break;
                }
            }
        }
        let Some(thefile) = file_path else {
            return Vec::with_capacity(0);
        };

        let mut list = Vec::new();
        fn walk(list: &mut Vec<String>, db: &Database, config: &Config, path: &LocalPath) {
            let Ok(serve) = ServePath::from_local_path(db, config, path) else {
                // TODO error xdd
                tracing::error!("serve path");
                return;
            };

            if path == &db.thumbnail_dir {
                return;
            }

            list.push(serve.to_string(false));
            if path.local_path().is_dir() {
                let Ok(readdir) = path.local_path().read_dir() else {
                    // TODO error xdd
                    tracing::error!("couldn't read dir");
                    return;
                };
                for child in readdir {
                    let Ok(child) = child else {
                        // TODO error xdd
                        tracing::error!("couldn't read entry");
                        continue;
                    };

                    walk(list, db, config, &LocalPath(child.path()));
                }
            }
        }
        walk(&mut list, self, config, thefile);
        list
    }
}

#[derive(Debug)]
pub struct Config {
    bind: String,
    auth: Option<String>,
    thumbnail_dir: String,
    file_dir: String,
    thumbnail_size: u32,
    rebuild_thumbnails: bool,
    page_root: Option<String>,
    auth_realm: Option<String>,
}

impl Config {
    fn read_from(config_path: &str) -> Result<Config, Error> {
        let config_file = std::fs::read_to_string(config_path)
            .with_context(|| format!("Reading path {}", config_path))?;
        let toml = toml::from_str::<toml::Value>(&config_file)
            .with_context(|| format!("Parsing TOML data in {}", config_path))?;

        let thumbnail_dir = toml
            .get("thumbnail_dir")
            .ok_or(ErrorInner::Config("thumbnail_dir missing in config file"))?
            .as_str()
            .ok_or(ErrorInner::Config(
                "thumbnail dir must be string in config file",
            ))?
            .to_string();

        let file_dir = toml
            .get("file_dir")
            .ok_or(ErrorInner::Config("need file dir in config file"))?
            .as_str()
            .ok_or(ErrorInner::Config("file dir must be string in config file"))?
            .to_string();

        let thumbnail_size = toml
            .get("thumbnail_size")
            .map(|size| match size {
                toml::Value::Integer(value) => (*value).try_into().map_err(ErrorInner::from),
                _ => Err(ErrorInner::Config("thumbnail size must be integer")),
            })
            .transpose()?
            .unwrap_or(75);

        let page_root = toml
            .get("page_root")
            .map(|page| {
                page.as_str()
                    .map(String::from)
                    .map(|value| {
                        if !value.starts_with("/") {
                            format!("/{}", value)
                        } else {
                            value
                        }
                    })
                    .ok_or(ErrorInner::Config(
                        "page_root must be a string in config file",
                    ))
            })
            .transpose()?;

        let auth = toml
            .get("auth")
            .map(|auth| {
                auth.as_str()
                    .map(String::from)
                    .ok_or(ErrorInner::Config("auth must be a string in config file"))
            })
            .transpose()?;

        let bind = toml
            .get("bind")
            .ok_or(ErrorInner::Config("need bind in config file"))?
            .as_str()
            .ok_or(ErrorInner::Config("bind must be string in config file"))?
            .to_string();

        let auth_realm = toml
            .get("auth_realm")
            .map(|realm| {
                realm
                    .as_str()
                    .map(String::from)
                    .ok_or(ErrorInner::Config("auth_realm must be a string"))
            })
            .transpose()?;

        Ok(Config {
            bind,
            auth,
            thumbnail_dir,
            file_dir,
            thumbnail_size,
            rebuild_thumbnails: false,
            page_root,
            auth_realm,
        })
    }
}

fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();

    let args = std::env::args().collect::<Vec<_>>();
    let mut config = Config::read_from(
        args.get(1)
            .ok_or(ErrorInner::Config("need config file argument"))?,
    )?;
    let database = Database::read_config_and_make_dirs(&config)?;

    tracing::debug!("{:#?}", config);
    tracing::debug!("{:#?}", database);
    tracing::info!("checking thumbnail database");

    config.rebuild_thumbnails = Some("--rebuild-thumbnails") == args.get(2).map(|s| &**s);
    database.index_and_build_thumbnail_db(&config)?;

    tracing::info!("starting! binding to {}", config.bind);

    // hmmmmmmm
    let db: &Database = Box::leak(Box::new(database));

    let mut tera = Tera::default();
    tera.add_raw_template("page", PAGE_TEMPLATE).unwrap();
    let tera: &Tera = Box::leak(Box::new(tera));

    rouille::start_server(config.bind.clone(), move |request| {
        let remote = request
            .header("X-Real-IP")
            .map(String::from)
            .unwrap_or_else(|| request.remote_addr().to_string());
        let full_url = request.url();
        tracing::debug!("new request from {}: {}", remote, full_url);

        if let Some(config_auth) = &config.auth {
            if let Some(auth_value) = request.header("Authorization") {
                let auth = auth_value.split(" ").collect::<Vec<_>>();
                if auth.len() != 2 {
                    tracing::warn!("broken auth header: {}", auth_value);
                    return bad_request("Broken auth header");
                }
                if auth[0] != "Basic" {
                    tracing::warn!("broken auth type: {}", auth[0]);
                    return bad_request("Incorrect auth type");
                }
                use base64::Engine;
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&auth[1]) else {
                    tracing::warn!("broken auth: {}", auth[1]);
                    return bad_request("Invalid Basic auth");
                };
                let Ok(auth) = std::str::from_utf8(&bytes) else {
                    tracing::warn!("broken auth utf8: {}", auth[1]);
                    return bad_request("Basic auth not UTF-8");
                };
                if auth != config_auth {
                    tracing::warn!("incorrect user/pass from {}: {}", remote, auth);
                    return unauthorized("Incorrect user/pass");
                }
            } else {
                return Response::text("need auth!")
                    .with_status_code(401)
                    .with_unique_header(
                        "WWW-Authenticate",
                        format!(
                            "Basic realm=\"{}\"",
                            config
                                .auth_realm
                                .as_ref()
                                .map(|s| s.as_str())
                                .unwrap_or("dop")
                        ),
                    );
            }
        }

        if Some(&full_url) == config.page_root.as_ref() {
            if let Some(thumbnail) = request.get_param("thumbnail") {
                tracing::trace!("Thumbnail request for {}", thumbnail);
                let Ok(thumb) = db.open_thumbnail(&thumbnail) else {
                    tracing::error!("couldn't read thumbnail {}", thumbnail);
                    return internal_error();
                };
                return Response::from_file("image/jpeg", thumb)
                    .with_unique_header("Cache-Control", "public, max-age=604800, immutable");
            }
        }

        let url = if let Some(root) = config.page_root.as_ref() {
            if !full_url.starts_with(root) {
                tracing::debug!("url didn't start with page root");
                return bad_request("URL path did not start with page root");
            }

            &full_url
        } else {
            &full_url
        };

        let url_serve_path = ServePath::from(PathBuf::from(url));
        let request_local_path = match LocalPath::from_serve_path(&db, &config, &url_serve_path) {
            Ok(local_path) => local_path,
            Err(error) if matches!(error.inner_ref(), &ErrorInner::FileNotFound(_)) => {
                return not_found()
            }
            _ => return unauthorized("Not a local path"),
        };

        tracing::debug!(
            "path looks like {}",
            request_local_path.local_path().display()
        );

        if request_local_path
            .local_path()
            .ancestors()
            .all(|parent| parent != db.file_dir.local_path())
            && request_local_path
                .local_path()
                .ancestors()
                .all(|parent| parent != db.thumbnail_dir.local_path())
        {
            tracing::warn!(
                "preventing directory traversal: {} tried to access {}",
                remote,
                request_local_path
                    .local_path()
                    .canonicalize()
                    .unwrap_or(PathBuf::from("(couldn't canonicalize)"))
                    .display()
            );
            return unauthorized("Unauthorized");
        }

        tracing::debug!(
            "serving {} on \"{}\"",
            request_local_path.local_path().display(),
            url
        );

        if request_local_path.local_path().is_dir() {
            if let Some(_) = request.get_param("filelist") {
                tracing::debug!("asked for file list");
                let file_list = db.file_list_in(&config, &request_local_path);
                return Response::json(&file_list);
            }

            if let Ok(mut context) = db.get_context_for(&config, &url_serve_path) {
                let ancestors = request_local_path
                    .local_path()
                    .ancestors()
                    .take_while(|parent| *parent != db.file_dir.local_path().parent().unwrap())
                    .collect::<Vec<_>>();

                #[derive(Serialize, Debug)]
                struct TitlePart {
                    href: String,
                    path: String,
                    last: bool,
                }

                let Ok(mut title_parts) = ancestors
                    .into_iter()
                    .rev()
                    .enumerate()
                    .map(|(i, unc)| {
                        let path = if i == 0 {
                            unc.display().to_string()
                        } else {
                            unc.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        };
                        Ok::<_, Error>(TitlePart {
                            href: ServePath::from_local_path(
                                &db,
                                &config,
                                &LocalPath::from(unc.to_path_buf()),
                            )
                            .with_context(|| {
                                format!(
                                    "Creating title part for ancestor {} of {}",
                                    unc.display(),
                                    url_serve_path
                                )
                            })?
                            .to_string(true),
                            path,
                            last: false,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
                else {
                    return internal_error();
                };

                if let Some(last) = title_parts.last_mut() {
                    last.last = true;
                };

                context.insert(
                    "tab_title",
                    &request_local_path.local_path().display().to_string(),
                );
                context.insert("page_title_parts", &title_parts);
                context.insert("page_root", config.page_root.as_deref().unwrap_or(""));

                match tera.render("page", &context) {
                    Ok(page) => Response::from_data("text/html", page),
                    Err(err) => {
                        Response::text(format!("frigk: {:?}", err.source())).with_status_code(500)
                    }
                }
            } else {
                internal_error()
            }
        } else {
            let Ok(file) = std::fs::File::open(request_local_path.local_path()) else {
                return not_found();
            };

            let extension = request_local_path
                .local_path()
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase());

            Response::from_file(
                match extension.as_ref().map(|s| s.as_str()) {
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("png") => "image/png",
                    Some("tiff" | "tif") => "image/tiff",
                    Some("bmp") => "image/bmp",
                    Some("gif") => "image/gif",
                    Some("txt") => "text/plain",
                    Some("svg") => "image/svg+xml",
                    Some("pdf") => "application/pdf",
                    _ => "application/binary",
                },
                file,
            )
        }
    });
}
