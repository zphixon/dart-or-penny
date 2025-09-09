use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode, Uri},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    Json, Router,
};
use axum_extra::{
    headers::{authorization::Basic, Authorization},
    TypedHeader,
};
use image::{buffer::ConvertBuffer, ImageBuffer, ImageReader, Rgb};
use percent_encoding::percent_decode;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    error::Error as StdError,
    fmt::Display,
    net::SocketAddr,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};
use tera::{Context as TeraContext, Tera};
use thiserror::Error as ThisError;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tower_http::compression::CompressionLayer;

const PAGE_TEMPLATE: &str = include_str!("./page.html.tera");

#[derive(ThisError, Debug)]
enum ErrorInner {
    #[error("Could not strip prefix: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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
    #[error("Regex: {0}")]
    Regex(#[from] regex::Error),
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

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        fn get_code_text(err: Error) -> (StatusCode, String) {
            match err {
                Error::Root(error_inner) => match error_inner {
                    ErrorInner::StripPrefix(_)
                    | ErrorInner::Io(_)
                    | ErrorInner::ThumbnailDirNotDir
                    | ErrorInner::FileDirNotDir
                    | ErrorInner::CannotServeFromRoot
                    | ErrorInner::Image(_)
                    | ErrorInner::Config(_)
                    | ErrorInner::NumberParse(_)
                    | ErrorInner::FromToml(_) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("{}", error_inner),
                    ),
                    ErrorInner::Regex(_) => (StatusCode::BAD_REQUEST, format!("{}", error_inner)),
                },
                Error::Context { context, inner } => {
                    let (code, text) = get_code_text(*inner);
                    (code, context + "\n" + &text)
                }
            }
        }
        get_code_text(self).into_response()
    }
}

fn thumbnail_filename(of: &Path) -> String {
    let name = format!("{}", of.display());
    let mut hasher = md5_rs::Context::new();
    hasher.read(name.as_bytes());
    hasher
        .finish()
        .into_iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>()
        + ".webp"
}

#[derive(Debug)]
enum MyFile {
    Dir(PathBuf, Vec<MyFile>),
    File(PathBuf),
}

impl MyFile {
    const THUMBNAILABLE_EXTENSIONS: &'static [&'static str] =
        &["png", "tiff", "bmp", "gif", "jpeg", "jpg", "tif", "webp"];

    fn walk_dir(dir: &Path, include_path: &impl Fn(&Path) -> bool) -> Result<Vec<MyFile>, Error> {
        let mut contents = Vec::new();
        for entry in dir
            .read_dir()
            .with_context(|| format!("Reading entries in {}", dir.display()))?
        {
            let entry = entry.with_context(|| format!("Reading dir entry in {}", dir.display()))?;
            let path = entry.path().canonicalize().with_context(|| {
                format!(
                    "Canonicalizing {} in {}",
                    entry.path().display(),
                    dir.display()
                )
            })?;

            if include_path(&path) {
                contents.push(if path.is_dir() {
                    let inner = Self::walk_dir(&path, include_path)
                        .with_context(|| format!("Walking entries in {}", path.display()))?;
                    MyFile::Dir(path, inner)
                } else {
                    MyFile::File(path)
                });
            }
        }

        Ok(contents)
    }

    fn may_be_thumbnailed(&self) -> bool {
        match self {
            MyFile::Dir(..) => false,
            MyFile::File(file) => {
                let Some(ext) = file.extension() else {
                    return false;
                };

                let ext = ext.to_string_lossy().to_lowercase();
                Self::THUMBNAILABLE_EXTENSIONS.contains(&ext.as_str())
            }
        }
    }

    fn find(&self, local_path: &Path) -> Option<&MyFile> {
        match self {
            MyFile::Dir(my_local_path, vec) => {
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
            MyFile::File(my_local_path) => {
                if my_local_path == local_path {
                    Some(self)
                } else {
                    None
                }
            }
        }
    }

    fn path(&self) -> &Path {
        match self {
            MyFile::Dir(local_path, _) => local_path,
            MyFile::File(local_path) => local_path,
        }
    }

    fn len(&self) -> usize {
        match self {
            MyFile::Dir(_, my_files) => my_files.iter().map(|file| file.len()).sum(),
            MyFile::File(_) => 1,
        }
    }
}

fn build_thumbnail_db(
    files: &[MyFile],
    thumbnail_dir: &Path,
) -> Result<HashMap<PathBuf, String>, Error> {
    fn btdb_rec(
        db: &mut HashMap<PathBuf, String>,
        files: &[MyFile],
        thumbnail_dir: &Path,
    ) -> Result<(), Error> {
        for file in files {
            match file {
                MyFile::Dir(_, files) => btdb_rec(db, files, thumbnail_dir).with_context(|| {
                    format!("Building thumbnail DB in {}", thumbnail_dir.display())
                })?,
                file @ MyFile::File(path) if file.may_be_thumbnailed() => {
                    let path = path.canonicalize().with_context(|| {
                        format!("Canonicalizing thumbnail path at {}", path.display())
                    })?;
                    let thumbnail_filename = thumbnail_filename(&path);
                    db.insert(path, thumbnail_filename);
                }
                MyFile::File(path) => {
                    tracing::debug!("skipping thumbnail for {}", path.display());
                }
            }
        }

        Ok(())
    }

    let mut db = HashMap::new();
    btdb_rec(&mut db, files, thumbnail_dir)
        .with_context(|| format!("Building outer thumbnail DB in {}", thumbnail_dir.display()))?;
    Ok(db)
}

#[derive(Debug)]
pub struct Database {
    file_dir: PathBuf,
    #[allow(dead_code)]
    files: Vec<MyFile>,
    thumbnail_dir: PathBuf,
    thumbnails: HashMap<PathBuf, String>,
}

impl Database {
    async fn open_thumbnail(&self, thumb: &str) -> Result<tokio::fs::File, Error> {
        let thumbnail_path = self.thumbnail_dir.join(thumb);
        Ok(tokio::fs::File::open(thumbnail_path)
            .await
            .with_context(|| format!("Reading thumbnail {}", thumb))?)
    }

    fn get_context_for(&self, config: &Config, serve_dir: &Path) -> Result<TeraContext, Error> {
        let mut context = TeraContext::new();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in serve_dir.read_dir().with_context(|| {
            format!(
                "Reading dir entry building context in {}",
                serve_dir.display()
            )
        })? {
            let entry =
                entry.with_context(|| format!("Reading dir entry in {}", serve_dir.display()))?;
            let path = entry.path();

            if path == self.thumbnail_dir {
                continue;
            }

            let basename = if let Some(basename) = path
                .file_name()
                .map(|osstr| osstr.to_string_lossy().to_string())
            {
                basename
            } else {
                String::from("<unknown>")
            };

            if path.is_dir() {
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
            thumbnail_filename: Option<String>,
        }

        let mut serde_dirs = Vec::new();
        for (path, basename) in dirs.into_iter() {
            let meta = path.metadata();
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
                filename: format!(
                    "{}/{}",
                    config.page_root.as_deref().unwrap_or_default(),
                    path.strip_prefix(&config.file_dir)?.display()
                ),
                thumbnail_filename: None,
            });
        }

        let mut serde_files = Vec::new();
        for (path, basename) in files.into_iter() {
            let meta = path.metadata();
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
                filename: format!(
                    "{}/{}",
                    config.page_root.as_deref().unwrap_or_default(),
                    path.strip_prefix(&config.file_dir)?.display()
                ),
                thumbnail_filename: self.thumbnails.get(&path).cloned().into_iter().next(),
            });
        }

        context.insert("dirs", &serde_dirs);
        context.insert("files", &serde_files);

        if serve_dir == config.file_dir {
            context.insert(
                "num_files",
                &self.files.iter().map(|myfile| myfile.len()).sum::<usize>(),
            );
        } else {
            context.insert(
                "num_files",
                &self
                    .files
                    .iter()
                    .flat_map(|myfile| myfile.find(serve_dir))
                    .map(|myfile| myfile.len())
                    .next()
                    .unwrap_or(0),
            );
        }

        context.insert(
            "path_sep",
            if cfg!(target_os = "windows") {
                "\\\\"
            } else {
                "/"
            },
        );
        context.insert(
            "file_dir",
            &config.file_dir.display().to_string().replace("\\", "\\\\"),
        );
        Ok(context)
    }

    fn read_config_and_make_dirs(config: &Config) -> Result<Database, Error> {
        std::fs::create_dir_all(&config.thumbnail_dir)?;
        let thumbnail_dir = config.thumbnail_dir.canonicalize().with_context(|| {
            format!(
                "Canonicalizing thumbnail dir {}",
                config.thumbnail_dir.display()
            )
        })?;
        if !thumbnail_dir.exists() {
            std::fs::create_dir(&thumbnail_dir).context("Creating thumbnail dir")?;
        } else if !thumbnail_dir.is_dir() {
            return Err(ErrorInner::ThumbnailDirNotDir.into());
        }

        let file_dir = config
            .file_dir
            .canonicalize()
            .context("Canonicalizing file dir")?;
        if !file_dir.exists() {
            std::fs::create_dir(&file_dir).context("Creating file dir")?;
        } else if !file_dir.is_dir() {
            return Err(ErrorInner::FileDirNotDir.into());
        }
        if file_dir.parent().is_none() {
            return Err(ErrorInner::CannotServeFromRoot.into());
        }

        let files = MyFile::walk_dir(&file_dir, &|path| path != &thumbnail_dir)
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
            let thumbnail_path = config.thumbnail_dir.join(thumbnail_path);
            let thumbnail_path = thumbnail_path.as_path();
            if !thumbnail_path.exists() || config.rebuild_thumbnails {
                tracing::info!(
                    "making thumbnail for {} -> {}",
                    file_path.display(),
                    thumbnail_path.display()
                );

                let image = match ImageReader::open(&file_path)
                    .with_context(|| format!("Reading image {}", file_path.display()))?
                    .with_guessed_format()
                    .with_context(|| format!("Guessing format of {}", file_path.display()))?
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
                let dynamic = image::DynamicImage::from(converted);
                let Ok(encoder) = webp::Encoder::from_image(&dynamic) else {
                    tracing::error!("Couldn't encode {} as webp", file_path.display());
                    continue;
                };
                let webp = encoder.encode(60.0);
                std::fs::write(thumbnail_path, &*webp)
                    .with_context(|| format!("Saving thumbnail {}", thumbnail_path.display()))?;
            }
        }
        Ok(())
    }

    fn file_list_matching(&self, config: &Config, include: impl Fn(&Path) -> bool) -> Vec<String> {
        let mut list = Vec::new();
        fn walk(
            list: &mut Vec<String>,
            db: &Database,
            config: &Config,
            include: &impl Fn(&Path) -> bool,
            path: &Path,
        ) {
            if path == &db.thumbnail_dir {
                return;
            }

            if path != &config.file_dir {
                let Ok(strip_path) = path.strip_prefix(&config.file_dir) else {
                    tracing::error!("couldn't strip prefix");
                    return;
                };

                // path?
                if include(strip_path) {
                    list.push(strip_path.display().to_string());
                }
            }

            if path.is_dir() {
                let Ok(readdir) = path.read_dir() else {
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

                    walk(list, db, config, include, &child.path());
                }
            }
        }
        walk(&mut list, self, config, &include, &self.file_dir);
        list
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    bind: SocketAddr,
    thumbnail_dir: PathBuf,
    file_dir: PathBuf,
    thumbnail_size: u32,
    #[serde(default)]
    rebuild_thumbnails: bool,
    page_root: Option<String>,
    basic_auth: Option<BasicAuthConfig>,
}

#[derive(Debug, Deserialize)]
struct BasicAuthConfig {
    user: String,
    password: String,
    realm: Option<String>,
}

#[derive(Clone)]
struct AppState {
    db: Arc<Database>,
    tera: Arc<Tera>,
    config: Arc<Config>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();

    let args = std::env::args().collect::<Vec<_>>();
    let config_file = args
        .get(1)
        .ok_or(ErrorInner::Config("need config file argument"))?;

    let mut config: Config = toml::from_str(&std::fs::read_to_string(config_file)?)?;
    config.file_dir = config.file_dir.canonicalize()?;
    if !config.file_dir.is_dir() {
        tracing::error!("File dir is not a dir");
        return Err(ErrorInner::FileDirNotDir.into());
    }

    let db = Database::read_config_and_make_dirs(&config)?;

    tracing::debug!("{:#?}", config);
    tracing::debug!("{:#?}", db);
    tracing::info!("checking thumbnail database");

    config.rebuild_thumbnails = Some("--rebuild-thumbnails") == args.get(2).map(|s| &**s);
    db.index_and_build_thumbnail_db(&config)?;

    tracing::info!("starting! binding to {}", config.bind);

    let mut tera = Tera::default();
    tera.add_raw_template("page", PAGE_TEMPLATE).unwrap();

    let state = AppState {
        db: Arc::new(db),
        tera: Arc::new(tera),
        config: Arc::new(config),
    };

    let page_root = state.config.page_root.clone().unwrap_or(String::new());
    let search_endpoint = page_root.clone() + "/search";
    let thumbnail_endpoint = page_root + "/thumbnail/{thumbnail}";

    let app = Router::new()
        .fallback(file_handler)
        .route(&thumbnail_endpoint, axum::routing::get(thumbnail_handler))
        .route(&search_endpoint, axum::routing::get(search_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            basic_auth_layer,
        ))
        .layer(CompressionLayer::new())
        .with_state(state.clone());

    let listener = TcpListener::bind(state.config.bind)
        .await
        .with_context(|| format!("Binding to {}", state.config.bind))?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn basic_auth_layer(
    State(state): State<AppState>,
    basic_auth: Option<TypedHeader<Authorization<Basic>>>,
    request: Request,
    next: Next,
) -> Response {
    match (state.config.basic_auth.as_ref(), basic_auth) {
        (Some(BasicAuthConfig { user, password, .. }), Some(TypedHeader(header))) => {
            if header.username() == user && header.password() == password {
                tracing::trace!("Successful basic auth");
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "Incorrect username/password").into_response()
            }
        }

        (Some(BasicAuthConfig { realm, .. }), None) => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                &format!("Basic realm=\"{}\"", realm.as_deref().unwrap_or("dop")),
            )],
            "Need auth",
        )
            .into_response(),

        (None, _) => next.run(request).await,
    }
}

const THUMBNAIL_CACHE_POLICY: &str = "private, max-age=604800, immutable";
const DIR_PAGE_CACHE_POLICY: &str = "private, max-age=3600, must-revalidate";

async fn thumbnail_handler(
    State(state): State<AppState>,
    axum::extract::Path(thumbnail): axum::extract::Path<String>,
) -> Response {
    tracing::trace!("thumbnail: {}", thumbnail);
    let Ok(mut thumb) = state.db.open_thumbnail(&thumbnail).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read thumbnail",
        )
            .into_response();
    };

    let mut data = Vec::new();
    match thumb.read_to_end(&mut data).await {
        Ok(_) => {}
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not read thumbnail: {}", err),
            )
                .into_response()
        }
    }

    return (
        [
            ("Content-Type", "image/webp"),
            ("Cache-Control", THUMBNAIL_CACHE_POLICY),
        ],
        data,
    )
        .into_response();
}

#[derive(Deserialize, Debug)]
struct Search {
    regex: String,
    case_insensitive: Option<bool>,
}

async fn search_handler(
    State(state): State<AppState>,
    Query(search): Query<Search>,
) -> Result<Response, Error> {
    tracing::trace!("search: {:?}", search);

    let re = RegexBuilder::new(&search.regex)
        .unicode(true)
        .case_insensitive(search.case_insensitive.unwrap_or(true))
        .build()?;

    Ok(
        Json(state.db.file_list_matching(&state.config, |path: &Path| {
            re.is_match(&path.display().to_string())
        }))
        .into_response(),
    )
}

async fn file_handler(State(state): State<AppState>, uri: Uri) -> Response {
    tracing::trace!("path: {:?}", uri.path());

    let page_root = state.config.page_root.as_deref().unwrap_or("/");
    let Some(request_path_str) = uri.path().strip_prefix(page_root) else {
        return (StatusCode::NOT_FOUND, "Path request not start with root").into_response();
    };

    let request_path = request_path_str
        .split("/")
        .filter(|part| !part.is_empty())
        .fold(PathBuf::new(), |acc, next| {
            acc.join(
                percent_decode(next.as_bytes())
                    .decode_utf8_lossy()
                    .to_string(),
            )
        });

    let not_found = (
        StatusCode::NOT_FOUND,
        format!("Not found: {}", request_path.display()),
    )
        .into_response();

    tracing::trace!("Requested {}", request_path.display());
    if request_path.components().any(|comp| {
        tracing::trace!("{:?}", comp);
        !matches!(comp, std::path::Component::Normal(_))
    }) {
        tracing::warn!("URL path traversal attempt >:(");
        return not_found;
    }
    let Ok(full_request_path) = state.config.file_dir.join(request_path).canonicalize() else {
        return not_found;
    };
    let Ok(_) = full_request_path.strip_prefix(&state.config.file_dir) else {
        tracing::warn!("Symlink path traversal attempt >:(");
        return not_found;
    };

    if full_request_path.is_dir() {
        if let Ok(mut context) = state.db.get_context_for(&state.config, &full_request_path) {
            let ancestors = full_request_path
                .ancestors()
                .take_while(|parent| *parent != state.db.file_dir.parent().unwrap())
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
                        href: format!(
                            "{}/{}",
                            state.config.page_root.as_deref().unwrap_or_default(),
                            unc.strip_prefix(&state.config.file_dir)?.display()
                        ),
                        path,
                        last: false,
                    })
                })
                .collect::<Result<Vec<_>, _>>()
            else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not break page folder into parts for title",
                )
                    .into_response();
            };

            if let Some(last) = title_parts.last_mut() {
                last.last = true;
            };

            context.insert("tab_title", &full_request_path.display().to_string());
            context.insert("page_title_parts", &title_parts);
            context.insert("page_root", state.config.page_root.as_deref().unwrap_or(""));

            match state.tera.render("page", &context) {
                Ok(page) => {
                    ([("Cache-Control", DIR_PAGE_CACHE_POLICY)], Html(page)).into_response()
                }
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("frigk: {:?}", err.source()),
                )
                    .into_response(),
            }
        } else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create context for directory template rendering",
            )
                .into_response();
        }
    } else {
        let Ok(mut file) = tokio::fs::File::open(&full_request_path).await else {
            return not_found;
        };

        let mut data = Vec::new();
        match file.read_to_end(&mut data).await {
            Ok(_) => {}
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not read file data",
                )
                    .into_response()
            }
        }

        (
            [(
                "Content-Type",
                mime_guess::from_path(&full_request_path)
                    .first_or_octet_stream()
                    .essence_str(),
            )],
            data,
        )
            .into_response()
    }
}
