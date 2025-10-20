use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{StatusCode, Uri, header},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use axum_extra::{
    TypedHeader,
    headers::{Authorization, authorization::Basic},
};
use base64::Engine;
use dashmap::{DashMap, DashSet};
use image::{ImageBuffer, ImageReader, Rgb, buffer::ConvertBuffer};
use moka::future::Cache;
use percent_encoding::percent_decode;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    error::Error as StdError,
    fmt::Display,
    fs::Metadata,
    net::SocketAddr,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tera::{Context as TeraContext, Tera};
use thiserror::Error as ThisError;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tower_http::compression::CompressionLayer;

#[derive(ThisError, Debug)]
enum ErrorInner {
    #[error("Could not strip prefix: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("File dir must be dir")]
    FileDirNotDir,
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
    #[error("Tokio join: {0}")]
    TokioJoin(#[from] tokio::task::JoinError),
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
                    | ErrorInner::FileDirNotDir
                    | ErrorInner::Image(_)
                    | ErrorInner::Config(_)
                    | ErrorInner::NumberParse(_)
                    | ErrorInner::TokioJoin(_)
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

const THUMBNAILABLE_EXTENSIONS: &'static [&'static str] =
    &["png", "tiff", "bmp", "gif", "jpeg", "jpg", "tif", "webp"];

async fn indexer(state: Arc<AppState2>) -> Result<(), Error> {
    let file_dir_part_name = state.config.file_dir.display().to_string();
    state.files.insert(
        file_dir_part_name.clone(),
        MyFile2 {
            full_path: state.config.file_dir.clone(),
            metadata: tokio::fs::metadata(&state.config.file_dir)
                .await
                .expect("file dir metadata"),
            thumbnail_name: None,
            items_in_subdirs: 0,
            child_items: HashSet::new(),
        },
    );

    let mut period = state.config.scan_interval.into();
    let mut interval = tokio::time::interval(period);
    let mut prev = interval.tick().await; // first tick returns immediately
    loop {
        tracing::debug!("walking");

        let mut removed =
            index_thumbnail_find_removed(state.clone(), &state.config.file_dir).await?;
        calculate_subdirs(state.clone(), &file_dir_part_name);
        clear_removed(state.clone(), &mut removed);

        state
            .rebuild_thumbnails
            .fetch_and(false, std::sync::atomic::Ordering::SeqCst);

        let next = interval.tick().await;
        let dt = next - prev;
        if dt > period {
            tracing::warn!(
                "scan took {}s, longer than configured {}s. increasing by {}s",
                dt.as_secs(),
                period.as_secs(),
                state.config.scan_interval.as_secs(),
            );
            period += state.config.scan_interval;
            interval = tokio::time::interval(period);
        }
        prev = next;
    }
}

async fn index_thumbnail_find_removed(
    state: Arc<AppState2>,
    dir: &Path,
) -> Result<HashSet<String>, Error> {
    let part_dir = if dir != state.config.file_dir {
        dir.strip_prefix(&state.config.file_dir)
            .with_context(|| format!("strip prefix {}", dir.display()))?
            .display()
            .to_string()
    } else {
        state.config.file_dir.display().to_string()
    };
    tracing::trace!("reading dir {} ({})", dir.display(), part_dir);

    let mut read_dir = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("read_dir {}", dir.display()))?;

    let mut removed = HashSet::new();

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .with_context(|| format!("next_entry {}", dir.display()))?
    {
        let entry_path = entry.path();
        tracing::trace!("looking at entry {}", entry_path.display());
        let Ok(metadata) = entry.metadata().await else {
            tracing::warn!("couldn't read metadata of {}", entry_path.display());
            continue;
        };

        if metadata.is_symlink() {
            tracing::warn!("symlinks not supported: {}", entry_path.display());
            continue;
        }

        let canon_entry_path = entry
            .path()
            .canonicalize() // necessary?
            .with_context(|| format!("canonicalize {}", entry_path.display()))?;

        if canon_entry_path == state.config.thumbnail_dir {
            continue;
        }

        let part_name = canon_entry_path
            .strip_prefix(&state.config.file_dir)
            .with_context(|| format!("strip prefix {}", canon_entry_path.display()))?
            .display()
            .to_string();

        let is_dir = metadata.is_dir();

        {
            let mut parent = state
                .files
                .get_mut(&part_dir)
                .expect("file parent should exist");
            parent.child_items.insert(part_name.clone());
        }

        let thumbnail_name = if let Some(ext) = entry_path.extension()
            && !state.thumbnail_broken.contains(&part_name)
            && THUMBNAILABLE_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str())
        {
            let thumbnail_name = thumbnail_filename(&canon_entry_path);
            let thumbnail_path = state.config.thumbnail_dir.join(&thumbnail_name);
            if !matches!(tokio::fs::try_exists(&thumbnail_path).await, Ok(true))
                || state
                    .rebuild_thumbnails
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                if let Err(e) = write_thumbnail(&entry_path, &thumbnail_path, &state.config).await {
                    tracing::warn!(
                        "couldn't create thumbnail for {}: {}",
                        entry_path.display(),
                        e
                    );
                    state.thumbnail_broken.insert(part_name.clone());
                    None
                } else {
                    Some(thumbnail_name)
                }
            } else {
                Some(thumbnail_name)
            }
        } else {
            None
        };

        let old = state.files.insert(
            part_name.clone(),
            MyFile2 {
                full_path: canon_entry_path,
                metadata,
                thumbnail_name,
                items_in_subdirs: 0,
                child_items: HashSet::with_capacity(0),
            },
        );

        if is_dir {
            Box::pin(index_thumbnail_find_removed(state.clone(), &entry_path)).await?;
        }

        if let Some(old) = old {
            let new = state.files.get(&part_name).expect("inserted file");
            removed.extend(old.child_items.difference(&new.child_items).cloned());
        };

        calculate_subdirs(state.clone(), &part_name);
    }

    Ok(removed)
}

async fn write_thumbnail(
    image_path: &Path,
    thumbnail_path: &Path,
    config: &Config,
) -> Result<(), Error> {
    tracing::trace!(
        "creating thumbnail for {} in {}",
        image_path.display(),
        thumbnail_path.display()
    );
    let image_data = tokio::fs::read(&image_path).await?;
    let image = ImageReader::new(std::io::Cursor::new(image_data))
        .with_guessed_format()?
        .decode()?;

    let nw = config.thumbnail_size;
    let nh = (config.thumbnail_size as f32 * (image.height() as f32 / image.width() as f32)) as u32;
    let thumbnail = image::imageops::thumbnail(&image, nw, nh);

    let converted: ImageBuffer<Rgb<u8>, _> = thumbnail.convert();
    let dynamic = image::DynamicImage::from(converted);
    let Ok(encoder) = webp::Encoder::from_image(&dynamic) else {
        return Err(ErrorInner::Config(
            "couldn't create a thumbnail i guess? the webp crate's error type is a string btw.",
        )
        .into());
    };
    let webp = Vec::<u8>::from(&*encoder.encode(60.0));

    tokio::fs::write(&thumbnail_path, webp).await?;
    tracing::info!(
        "thumbnailed {} to {}",
        image_path.display(),
        thumbnail_path.display()
    );

    Ok(())
}

fn thumbnail_filename(of: &Path) -> String {
    let name = format!("{}", of.display());
    let digest = md5::compute(name);
    format!("{:02x}.webp", digest)
}

fn calculate_subdirs(state: Arc<AppState2>, part_name: &String) {
    let items_in_subdirs = {
        let mut total = 0;
        let my_file = state.files.get(part_name).expect("file is inserted");
        for child in my_file.child_items.iter() {
            let Some(my_child) = state.files.get(child) else {
                tracing::warn!("child not inserted: {}", child);
                continue;
            };
            if my_child.metadata.is_dir() {
                total += my_child.child_items.len() as u64;
                total += my_child.items_in_subdirs;
            }
        }
        total
    };
    {
        let mut my_file = state.files.get_mut(part_name).expect("file is inserted");
        my_file.items_in_subdirs = items_in_subdirs;
    }
}

fn clear_removed(state: Arc<AppState2>, removed: &mut HashSet<String>) {
    tracing::debug!("removing {:#?}", removed);
    while !removed.is_empty() {
        let Some(item_key) = removed.iter().next() else {
            break;
        };
        tracing::debug!("removing {}", item_key);
        let Some((key, item)) = state.files.remove(item_key) else {
            continue;
        };
        removed.remove(&key);
        for child_item in item.child_items {
            removed.insert(child_item);
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    bind: SocketAddr,
    thumbnail_dir: PathBuf,
    file_dir: PathBuf,
    #[serde(default = "default_thumbnail_size")]
    thumbnail_size: u32,
    #[serde(default = "default_page_root")]
    page_root: String,
    #[serde(default)]
    basic_auth: Option<BasicAuthConfig>,
    #[serde(default)]
    shortcut: Vec<Shortcut>,
    #[serde(
        default = "scan_interval",
        deserialize_with = "de_scan_interval"
    )]
    scan_interval: Duration,
}

fn scan_interval() -> Duration {
    Duration::from_secs(120)
}

fn de_scan_interval<'de, D: serde::de::Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
    struct V;

    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Duration;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(formatter, "string")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            humantime::parse_duration(v).map_err(E::custom)
        }
    }

    de.deserialize_str(V)
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:8888".parse().unwrap()
}

fn default_thumbnail_size() -> u32 {
    75
}

fn default_page_root() -> String {
    "/".into()
}

#[derive(Debug, Deserialize)]
struct BasicAuthConfig {
    user: String,
    password: String,
    realm: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Shortcut {
    name: String,
    url: String,
}

#[derive(argh::FromArgs)]
#[argh(description = "Single-binary file server")]
struct Args {
    #[argh(positional, description = "config file")]
    config: PathBuf,
    #[argh(switch, description = "re-build thumbnail files")]
    rebuild_thumbnails: bool,
}

struct AppState2 {
    rebuild_thumbnails: AtomicBool,
    thumbnail_name_data: Cache<String, String>,
    thumbnail_broken: DashSet<String>,
    files: DashMap<String, MyFile2>,
    tera: Tera,
    config: Config,
}

#[derive(Debug)]
struct MyFile2 {
    full_path: PathBuf,
    metadata: Metadata,
    thumbnail_name: Option<String>,
    items_in_subdirs: u64,
    child_items: HashSet<String>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    match run().await {
        Ok(()) => {}
        Err(e) => println!("{}", e),
    }

    Ok(())
}

async fn run() -> Result<(), Error> {
    tracing_subscriber::fmt::init();

    let args: Args = argh::from_env();

    let mut config: Config =
        toml::from_str(&std::fs::read_to_string(&args.config).context("reading config file")?)?;
    config.file_dir = config.file_dir.canonicalize().context("finding file dir")?;
    if !config.file_dir.is_dir() {
        tracing::error!("File dir is not a dir");
        return Err(ErrorInner::FileDirNotDir.into());
    }
    config.page_root = String::from("/")
        + config
            .page_root
            .trim_start_matches("/")
            .trim_end_matches("/");

    tokio::fs::create_dir_all(&config.thumbnail_dir)
        .await
        .with_context(|| format!("creating thumbnail dir {}", config.thumbnail_dir.display()))?;
    config.thumbnail_dir = config.thumbnail_dir.canonicalize()?;

    let mut tera = Tera::default();
    tera.add_raw_template("page", include_str!("../frontend/src/page.html.tera"))
        .unwrap();
    tera.add_raw_template(
        "webmanifest",
        include_str!("../frontend/src/site.webmanifest.tera"),
    )
    .unwrap();

    let state = Arc::new(AppState2 {
        rebuild_thumbnails: AtomicBool::new(args.rebuild_thumbnails),
        thumbnail_name_data: Cache::new(8192),
        thumbnail_broken: DashSet::new(),
        files: DashMap::new(),
        tera,
        config,
    });

    let indexer_task = tokio::task::spawn({
        let state = state.clone();
        async move { indexer(state).await }
    });

    tracing::info!("starting! binding to {}", state.config.bind);

    let page_root = state.config.page_root.clone();
    let search_endpoint = page_root.clone() + "/.dop/search";
    let assets_endpoint = page_root.clone() + "/.dop/assets/{item}";

    let app = Router::new()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            basic_auth_layer,
        ))
        .route(&assets_endpoint, axum::routing::get(assets_handler))
        .route(&search_endpoint, axum::routing::get(search_handler))
        .fallback(file_handler)
        .layer(CompressionLayer::new())
        .with_state(state.clone());

    let listener = TcpListener::bind(state.config.bind)
        .await
        .with_context(|| format!("Binding to {}", state.config.bind))?;

    tokio::select! {
        result = axum::serve(listener, app) => Ok(result?),
        result = indexer_task => Ok(result??), // ??
    }
}

async fn basic_auth_layer(
    State(state): State<Arc<AppState2>>,
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

const CACHE_POLICY: &str = "private, max-age=3600, must-revalidate";

async fn assets_handler(
    State(state): State<Arc<AppState2>>,
    axum::extract::Path(item): axum::extract::Path<String>,
) -> Response {
    let item = item.as_str();
    if item == "site.webmanifest" {
        let mut context = tera::Context::new();
        context.insert("page_root", &state.config.page_root);
        context.insert("shortcuts", &state.config.shortcut);

        let Ok(manifest) = state.tera.render("webmanifest", &context) else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "couldn't render web manifest template",
            )
                .into_response();
        };

        return ([("Content-Type", "application/manifest+json")], manifest).into_response();
    }

    macro_rules! response {
        ($name:literal => $content_type:literal $file:literal) => {
            if item == $name {
                return (
                    [
                        ("Content-Type", $content_type),
                        ("Cache-Control", CACHE_POLICY),
                    ],
                    include_bytes!($file),
                )
                    .into_response();
            }
        };
    }

    response!("page.js" => "text/javascript" "../frontend/build/page.js");
    response!("page.css" => "text/css" "../frontend/src/page.css");
    response!("apple-touch-icon.png" => "image/png" "../frontend/assets/apple-touch-icon.png");
    response!("favicon-96x96.png" => "image/png" "../frontend/assets/favicon-96x96.png");
    response!("favicon.ico" => "image/x-icon" "../frontend/assets/favicon.ico");
    response!("favicon.svg" => "image/svg+xml" "../frontend/assets/favicon.svg");
    response!("web-app-manifest-192x192.png" => "image/png" "../frontend/assets/web-app-manifest-192x192.png");
    response!("web-app-manifest-512x512.png" => "image/png" "../frontend/assets/web-app-manifest-512x512.png");

    #[cfg(debug_assertions)]
    {
        response!("page.js.map" => "text/javascript" "../frontend/build/page.js.map")
    }

    StatusCode::NOT_FOUND.into_response()
}

#[derive(Deserialize, Debug)]
struct Search {
    regex: String,
    case_insensitive: Option<bool>,
}

async fn search_handler(
    State(state): State<Arc<AppState2>>,
    Query(search): Query<Search>,
) -> Result<Response, Error> {
    tracing::trace!("search: {:?}", search);

    let re = RegexBuilder::new(&search.regex)
        .unicode(true)
        .case_insensitive(search.case_insensitive.unwrap_or(true))
        .build()?;

    Ok(Json(file_list_matching(state, |path: &Path| {
        re.is_match(&path.display().to_string())
    }))
    .into_response())
}

fn file_list_matching(state: Arc<AppState2>, include: impl Fn(&Path) -> bool) -> Vec<String> {
    let mut results = Vec::new();
    for file in state.files.iter() {
        let Ok(test) = file.full_path.strip_prefix(&state.config.file_dir) else {
            tracing::warn!(
                "couldn't strip prefix while searching from {}",
                file.full_path.display()
            );
            continue;
        };
        if include(test) {
            results.push(test.display().to_string());
        }
    }
    results.sort();
    results
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct PageItem {
    kind: PageItemKind,
    basename: String,
    filename: String,
    created: String,
    modified: String,
    accessed: String,
    thumbnail_data: Option<String>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
enum PageItemKind {
    File,
    Dir,
}

async fn file_handler(State(state): State<Arc<AppState2>>, uri: Uri) -> Response {
    let not_found = (
        StatusCode::NOT_FOUND,
        Html(format!(
            r#"<!DOCTYPE html>
            <html><head>
            <meta name="viewport" content="width=device-width, initial-scale=1, minimal-ui">
            <body><a href="{}">↖ Home</a> - Not found: {}"#,
            state.config.page_root,
            uri.path()
        )),
    )
        .into_response();

    if !uri.path().starts_with(&state.config.page_root) {
        tracing::trace!("not in page root");
        return not_found;
    }

    let mut request_path = uri
        .path()
        .trim_start_matches(&state.config.page_root)
        .split("/")
        .filter(|part| !part.is_empty())
        .fold(PathBuf::new(), |path, part| {
            path.join(percent_decode(part.as_bytes()).decode_utf8_lossy().as_ref())
        })
        .display()
        .to_string();

    tracing::debug!("request: {:?}", request_path);
    if request_path.is_empty() {
        request_path = state.config.file_dir.display().to_string();
    }

    // no path traversal - only MyFiles in state.files are accessible, and are
    // only found by the indexer. the indexer does not traverse symlinks, and
    // ensures that the path on disk is a child of file_dir by `canonicalize`ing
    // and `strip_prefix`ing
    let Some(request_file) = state.files.get(&request_path) else {
        tracing::debug!("not found, normal style: {}", request_path);
        return not_found;
    };

    if request_file.metadata.is_dir() {
        if let Ok(mut context) = get_context(state.clone(), &request_file).await {
            let ancestors = request_file
                .full_path
                .ancestors()
                .take_while(|parent| *parent != state.config.file_dir.parent().unwrap())
                .collect::<Vec<_>>();

            #[derive(Serialize, Debug)]
            struct TitlePart {
                href: String,
                path: String,
            }

            let Ok(title_parts) = ancestors
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
                            state.config.page_root,
                            unc.strip_prefix(&state.config.file_dir)?.display()
                        ),
                        path,
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

            context.insert("tab_title", &request_file.full_path.display().to_string());
            context.insert("page_title_parts", &title_parts);
            context.insert("page_root", &state.config.page_root);

            match state.tera.render("page", &context) {
                Ok(page) => ([("Cache-Control", CACHE_POLICY)], Html(page)).into_response(),
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
        let Ok(mut file) = tokio::fs::File::open(&request_file.full_path).await else {
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
                    .into_response();
            }
        }

        fn make_response(mime: &str, data: Vec<u8>) -> axum::http::Response<axum::body::Body> {
            ([("Content-Type", mime)], data).into_response()
        }

        // guess from the path extension first, then try reading magic. otherwise give up and say it's bytes
        if let Some(mime) = mime_guess::from_path(&request_file.full_path).first() {
            // not a &'static str, hence the helper function
            tracing::trace!("got mime from path");
            make_response(mime.essence_str(), data)
        } else if let Some(mime) = infer::get(&data) {
            tracing::trace!("got mime from data");
            make_response(mime.mime_type(), data)
        } else {
            tracing::trace!("unknown mime");
            make_response(
                mime_guess::mime::APPLICATION_OCTET_STREAM.essence_str(),
                data,
            )
        }
    }
}

async fn get_context(state: Arc<AppState2>, request_file: &MyFile2) -> Result<TeraContext, Error> {
    let mut context = TeraContext::new();

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for child in request_file.child_items.iter() {
        let Some(child) = state.files.get(child) else {
            continue;
        };
        let basename = child
            .full_path
            .file_name()
            .map(|ostr| ostr.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from("<unknown>"));
        if child.metadata.is_dir() {
            dirs.push((child, basename));
        } else {
            files.push((child, basename));
        }
    }

    dirs.sort_by(|(_, basename1), (_, basename2)| basename1.cmp(basename2));
    files.sort_by(|(_, basename1), (_, basename2)| basename1.cmp(basename2));

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

    let mut serde_items = Vec::new();
    for (child_dir, basename) in dirs.into_iter() {
        let created = child_dir
            .metadata
            .created()
            .map(timestamp)
            .unwrap_or_default();
        let modified = child_dir
            .metadata
            .modified()
            .map(timestamp)
            .unwrap_or_default();
        let accessed = child_dir
            .metadata
            .accessed()
            .map(timestamp)
            .unwrap_or_default();

        serde_items.push(PageItem {
            kind: PageItemKind::Dir,
            basename,
            created,
            modified,
            accessed,
            filename: format!(
                "{}/{}",
                state.config.page_root,
                child_dir
                    .full_path
                    .strip_prefix(&state.config.file_dir)?
                    .display()
            ),
            thumbnail_data: None,
        });
    }

    for (child_file, basename) in files.into_iter() {
        let created = child_file
            .metadata
            .created()
            .map(timestamp)
            .unwrap_or_default();
        let modified = child_file
            .metadata
            .modified()
            .map(timestamp)
            .unwrap_or_default();
        let accessed = child_file
            .metadata
            .accessed()
            .map(timestamp)
            .unwrap_or_default();

        let thumbnail_data = if !state.thumbnail_broken.contains(child_file.key())
            && let Some(thumbnail_name) = child_file.thumbnail_name.as_ref()
        {
            match state.thumbnail_name_data.get(thumbnail_name).await {
                Some(hit) => Some(hit),
                None => {
                    match tokio::fs::read(state.config.thumbnail_dir.join(thumbnail_name)).await {
                        Ok(bytes) => {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                            state
                                .thumbnail_name_data
                                .insert(thumbnail_name.to_owned(), b64.clone())
                                .await;
                            Some(b64)
                        }
                        Err(_) => {
                            state.thumbnail_broken.insert(child_file.key().to_owned());
                            None
                        }
                    }
                }
            }
        } else {
            None
        };

        serde_items.push(PageItem {
            kind: PageItemKind::File,
            basename,
            created,
            modified,
            accessed,
            filename: format!(
                "{}/{}",
                state.config.page_root,
                child_file
                    .full_path
                    .strip_prefix(&state.config.file_dir)?
                    .display()
            ),
            thumbnail_data,
        });
    }

    context.insert("items", &serde_items);

    if request_file.full_path == state.config.file_dir {
        context.insert(
            "in_subdirs",
            &(request_file.child_items.len() as u64 + request_file.items_in_subdirs),
        );
    } else {
        context.insert("in_subdirs", &request_file.items_in_subdirs);
    }

    context.insert(
        "path_sep",
        if cfg!(target_os = "windows") {
            "\\"
        } else {
            "/"
        },
    );
    context.insert("file_dir", &state.config.file_dir.display().to_string());
    Ok(context)
}
