use anyhow::Result;
use image::{buffer::ConvertBuffer, ImageBuffer, ImageReader, Rgb};
use rouille::Response;
use serde::Serialize;
use std::{
    collections::HashMap,
    error::Error,
    fs::File as FsFile,
    path::{Path, PathBuf},
};
use tera::{Context, Tera};

mod path;

use path::{LocalPath, ServePath, ThumbnailPath};

const PAGE_TEMPLATE: &str = include_str!("./page.html.tera");

#[macro_export]
macro_rules! af {
    ($($tt:tt)*) => { {
        let msg = format!($($tt)*);
        ::tracing::error!("{}:{} {}", file!(), line!(), msg);
        ::anyhow::anyhow!(msg)
    } };
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

fn bad_request() -> Response {
    Response::text("Bad request").with_status_code(400)
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

    fn walk_dir(dir: &LocalPath, include_path: &impl Fn(&Path) -> bool) -> Result<Vec<File>> {
        let mut contents = Vec::new();
        for entry in dir
            .local_path()
            .read_dir()
            .map_err(|e| af!("couldn't walk dir {}: {}", dir.local_path().display(), e))?
        {
            let entry = entry.map_err(|e| {
                af!(
                    "couldn't read entry in {}: {}",
                    dir.local_path().display(),
                    e
                )
            })?;
            let path = entry.path().canonicalize().map_err(|e| {
                af!(
                    "couldn't get absolute path of {}: {}",
                    entry.path().display(),
                    e
                )
            })?;

            if include_path(&path) {
                contents.push(if path.is_dir() {
                    let local_path = LocalPath::from(path);
                    let inner = Self::walk_dir(&local_path, include_path)?;
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
) -> Result<HashMap<LocalPath, ThumbnailPath>> {
    fn btdb_rec(
        db: &mut HashMap<LocalPath, ThumbnailPath>,
        files: &[File],
        thumbnail_dir: &LocalPath,
    ) -> Result<()> {
        for file in files {
            match file {
                File::Dir(_, files) => btdb_rec(db, files, thumbnail_dir)?,
                file @ File::File(path) if file.may_be_thumbnailed() => {
                    let path = path.local_path().canonicalize().map_err(|e| {
                        af!(
                            "couldn't get absolute path for {}: {}",
                            path.local_path().display(),
                            e
                        )
                    })?;
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
    btdb_rec(&mut db, files, thumbnail_dir)?;
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
    fn open_thumbnail(&self, thumb: &str) -> Result<FsFile> {
        let thumbnail_path = self.thumbnail_dir.local_path().join(thumb);
        Ok(FsFile::open(thumbnail_path)?)
    }

    fn get_context_for(&self, config: &Config, serve_dir: &ServePath) -> Result<Context> {
        let local_dir = LocalPath::from_serve_path(&self, config, serve_dir)?;

        let mut context = Context::new();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in local_dir.local_path().read_dir().map_err(|e| {
            af!(
                "couldn't walk dir to make page: {}: {}",
                local_dir.local_path().display(),
                e
            )
        })? {
            let entry = entry.map_err(|e| {
                af!(
                    "couldn't read dir entry in {}: {}",
                    local_dir.local_path().display(),
                    e
                )
            })?;
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
                filename: ServePath::from_local_path(&self, config, &path)?.to_string(true),
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
                filename: ServePath::from_local_path(&self, config, &path)?.to_string(true),
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

    fn read_config_and_make_dirs(config: &Config) -> Result<Database> {
        let thumbnail_dir = PathBuf::from(&config.thumbnail_dir);
        let thumbnail_dir = LocalPath::from(thumbnail_dir.canonicalize().map_err(|e| {
            af!(
                "couldn't create absolute thumbnail dir from {}: {}",
                thumbnail_dir.display(),
                e
            )
        })?);
        if !thumbnail_dir.local_path().exists() {
            std::fs::create_dir(thumbnail_dir.local_path()).map_err(|e| {
                af!(
                    "couldn't create thumbnail dir {}: {}",
                    thumbnail_dir.local_path().display(),
                    e
                )
            })?;
        } else if !thumbnail_dir.local_path().is_dir() {
            return Err(af!(
                "thumbnail dir must be dir {}",
                thumbnail_dir.local_path().display()
            ));
        }

        let file_dir = PathBuf::from(&config.file_dir);
        let file_dir = LocalPath::from(file_dir.canonicalize().map_err(|e| {
            af!(
                "couldn't create absolute file dir from {}: {}",
                file_dir.display(),
                e
            )
        })?);
        if !file_dir.local_path().exists() {
            std::fs::create_dir(file_dir.local_path()).map_err(|e| {
                af!(
                    "couldn't create file dir {}: {}",
                    file_dir.local_path().display(),
                    e
                )
            })?;
        } else if !file_dir.local_path().is_dir() {
            return Err(af!(
                "file dir must be dir {}",
                file_dir.local_path().display()
            ));
        }
        if file_dir.local_path().parent().is_none() {
            return Err(af!("cannot serve files from root dir"));
        }

        let files = File::walk_dir(&file_dir, &|path| path != thumbnail_dir.local_path())?;
        let thumbnails = build_thumbnail_db(&files, &thumbnail_dir)?;
        Ok(Database {
            file_dir,
            files,
            thumbnail_dir,
            thumbnails,
        })
    }

    fn index_and_build_thumbnail_db(&self, config: &Config) -> Result<()> {
        for (file_path, thumbnail_path) in self.thumbnails.iter() {
            if !thumbnail_path.thumbnail_path().exists() || config.rebuild_thumbnails {
                tracing::info!(
                    "making thumbnail for {} -> {}",
                    file_path.local_path().display(),
                    thumbnail_path.thumbnail_path().display()
                );

                let image = match ImageReader::open(file_path.local_path())
                    .map_err(|e| {
                        af!(
                            "couldn't read file for thumbnailing: {}: {}",
                            file_path.local_path().display(),
                            e
                        )
                    })?
                    .with_guessed_format()
                    .map_err(|e| {
                        af!(
                            "couldn't guess format: {}: {}",
                            file_path.local_path().display(),
                            e
                        )
                    })?
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
                    .map_err(|e| {
                        af!(
                            "couldn't save thumbnail for {} in {}: {}",
                            file_path.local_path().display(),
                            thumbnail_path.thumbnail_path().display(),
                            e
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
    fn read_from(config_path: &str) -> Result<Config> {
        let config_file = std::fs::read_to_string(config_path)
            .map_err(|_| af!("can't read config file {}", config_path))?;
        let toml = toml::from_str::<toml::Value>(&config_file)
            .map_err(|e| af!("couldn't read config file {}:\n{:#?}", config_path, e))?;

        let thumbnail_dir = toml
            .get("thumbnail_dir")
            .ok_or_else(|| af!("need thumbnail dir in config file {}", config_path))?
            .as_str()
            .ok_or_else(|| {
                af!(
                    "thumbnail dir must be string in config file {}",
                    config_path
                )
            })?
            .to_string();

        let file_dir = toml
            .get("file_dir")
            .ok_or_else(|| af!("need file dir in config file {}", config_path))?
            .as_str()
            .ok_or_else(|| af!("file dir must be string in config file {}", config_path))?
            .to_string();

        let thumbnail_size = toml
            .get("thumbnail_size")
            .map(|size| match size {
                toml::Value::Integer(value) => (*value)
                    .try_into()
                    .map_err(|_| af!("thumbnail size must fit in u32")),
                _ => Err(af!("thumbnail size must be integer")),
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
                    .ok_or_else(|| af!("page_root must be a string in config file {}", config_path))
            })
            .transpose()?;

        let auth = toml
            .get("auth")
            .map(|auth| {
                auth.as_str()
                    .map(String::from)
                    .ok_or_else(|| af!("auth must be a string in config file {}", config_path))
            })
            .transpose()?;

        let bind = toml
            .get("bind")
            .ok_or_else(|| af!("need bind in config file {}", config_path))?
            .as_str()
            .ok_or_else(|| af!("bind must be string in config file {}", config_path))?
            .to_string();

        let auth_realm = toml
            .get("auth_realm")
            .map(|realm| {
                realm
                    .as_str()
                    .map(String::from)
                    .ok_or_else(|| af!("auth_realm must be a string"))
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

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = std::env::args().collect::<Vec<_>>();
    let mut config = Config::read_from(
        args.get(1)
            .ok_or_else(|| af!("need config file argument"))?,
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
                    return bad_request();
                }
                if auth[0] != "Basic" {
                    tracing::warn!("broken auth type: {}", auth[0]);
                    return bad_request();
                }
                use base64::Engine;
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&auth[1]) else {
                    tracing::warn!("broken auth: {}", auth[1]);
                    return bad_request();
                };
                let Ok(auth) = std::str::from_utf8(&bytes) else {
                    tracing::warn!("broken auth utf8: {}", auth[1]);
                    return bad_request();
                };
                if auth != config_auth {
                    tracing::warn!("incorrect user/pass from {}: {}", remote, auth);
                    return bad_request();
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
                return bad_request();
            }

            &full_url
        } else {
            &full_url
        };

        let url_serve_path = ServePath::from(PathBuf::from(url));
        let Ok(request_local_path) = LocalPath::from_serve_path(&db, &config, &url_serve_path)
        else {
            return bad_request();
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
            return bad_request();
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
                        Ok::<_, anyhow::Error>(TitlePart {
                            href: ServePath::from_local_path(
                                &db,
                                &config,
                                &LocalPath::from(unc.to_path_buf()),
                            )?
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
