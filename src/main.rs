use anyhow::{anyhow, Result};
use image::io::Reader as ImageReader;
use rouille::Response;
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    fs::File as FsFile,
    path::{Path, PathBuf},
    sync::RwLock,
};

macro_rules! af {
    ($($tt:tt)*) => { {
        let msg = format!($($tt)*);
        tracing::error!("{}:{} {}", file!(), line!(), msg);
        anyhow!(msg)
    } };
}

#[derive(Default)]
struct Page {
    title: String,
    content: String,
    code: Option<u16>,
}

impl Page {
    fn with_title<S: ToString>(self, title: S) -> Self {
        Page {
            title: title.to_string(),
            ..self
        }
    }

    fn with_content<S: ToString>(self, content: S) -> Self {
        Page {
            content: content.to_string(),
            ..self
        }
    }

    fn with_paragraph<S: ToString>(self, para: S) -> Self {
        Page {
            content: format!("<p>{}</p>", para.to_string()),
            ..self
        }
    }

    fn with_code(self, code: u16) -> Self {
        Page {
            code: Some(code),
            ..self
        }
    }

    fn render(self, config: &Config) -> Response {
        let title = if let Some(code) = self.code {
            format!("{}: {}", code, self.title)
        } else {
            self.title
        };

        Response::html(format!(
            r#"<!DOCTYPE html>
<html>
  <head>
    <title>{}</title>
    <style>
      h1 {{ color: green; }}
      .thumbnail {{
        width: {}px;
      }}
      table, td, th {{
        border: 1px solid #090;
        border-collapse: collapse;
        padding-left: 4pt;
        padding-right: 8pt;
      }}
      table {{
        width: 80%;
      }}
    </style>
  </head>
  <body>
    <h1>{}</h1>
    {}
  </body>
</html>
"#,
            title, config.thumbnail_size, title, self.content
        ))
        .with_status_code(self.code.unwrap_or(200))
    }

    fn not_found(config: &Config) -> Response {
        Self::default()
            .with_title("not found")
            .with_paragraph("skill issue")
            .with_code(404)
            .render(config)
    }

    fn bad_request(config: &Config) -> Response {
        Self::default()
            .with_title("bad request")
            .with_paragraph("skill issue")
            .with_code(400)
            .render(config)
    }

    fn internal_error(config: &Config) -> Response {
        Self::default()
            .with_title("internal server error")
            .with_paragraph("skill issue (on our end)")
            .with_code(500)
            .render(config)
    }
}

fn thumbnail_path(of: &Path, thumbnail_dir: &Path) -> PathBuf {
    let name = format!("{}", of.display());

    let mut hasher = md5_rs::Context::new();
    hasher.read(name.as_bytes());
    let hash = hasher
        .finish()
        .into_iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>();

    thumbnail_dir.join(hash).with_extension("jpg")
}

#[derive(Debug)]
enum File {
    Dir(PathBuf, Vec<File>),
    File(PathBuf),
}

impl File {
    const THUMBNAILABLE_EXTENSIONS: &[&'static str] =
        &["png", "tiff", "bmp", "gif", "jpeg", "jpg", "tif"];

    fn walk_dir(dir: &Path, include_path: &impl Fn(&Path) -> bool) -> Result<Vec<File>> {
        let mut contents = Vec::new();
        for entry in dir
            .read_dir()
            .map_err(|e| af!("couldn't walk dir {}: {}", dir.display(), e))?
        {
            let entry =
                entry.map_err(|e| af!("couldn't read entry in {}: {}", dir.display(), e))?;
            let path = entry.path().canonicalize().map_err(|e| {
                af!(
                    "couldn't get absolute path of {}: {}",
                    entry.path().display(),
                    e
                )
            })?;

            if include_path(&path) {
                contents.push(if path.is_dir() {
                    File::Dir(path.clone(), Self::walk_dir(&path, include_path)?)
                } else {
                    File::File(path)
                });
            }
        }

        Ok(contents)
    }

    fn may_be_thumbnailed(&self) -> bool {
        match self {
            File::Dir(..) => false,
            File::File(file) => {
                let Some(ext) = file.extension() else {
                    return false;
                };

                let ext = ext.to_string_lossy().to_lowercase();
                Self::THUMBNAILABLE_EXTENSIONS.contains(&ext.as_str())
            }
        }
    }
}

fn build_thumbnail_db(files: &[File], thumbnail_dir: &Path) -> Result<HashMap<PathBuf, PathBuf>> {
    fn btdb_rec(
        db: &mut HashMap<PathBuf, PathBuf>,
        files: &[File],
        thumbnail_dir: &Path,
    ) -> Result<()> {
        for file in files {
            match file {
                File::Dir(_, files) => btdb_rec(db, files, thumbnail_dir)?,
                file @ File::File(path) if file.may_be_thumbnailed() => {
                    let path = path.canonicalize().map_err(|e| {
                        af!("couldn't get absolute path for {}: {}", path.display(), e)
                    })?;
                    let thumbnail_path = thumbnail_path(&path, thumbnail_dir);
                    db.insert(path, thumbnail_path);
                }
                File::File(path) => {
                    tracing::debug!("skipping thumbnail for {}", path.display());
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
struct Database {
    file_dir: PathBuf,
    files: Vec<File>,
    thumbnail_dir: PathBuf,
    thumbnails: HashMap<PathBuf, PathBuf>,
    pages: RwLock<HashMap<PathBuf, String>>,
}

impl Database {
    fn open_thumbnail(&self, thumb: &str) -> Result<FsFile> {
        let thumbnail_path = self.thumbnail_dir.join(thumb);
        Ok(FsFile::open(thumbnail_path)?)
    }

    fn get_content_for(&self, config: &Config, dir: &Path) -> Result<Option<String>> {
        {
            let read = self
                .pages
                .read()
                .map_err(|e| af!("couldn't lock page cache for reading: {}", e))?;

            if read.contains_key(dir) {
                return Ok(read.get(dir).cloned());
            }
        }

        let mut page = String::from("<table>");

        let mut paths_with_filenames = dir
            .read_dir()
            .map_err(|e| af!("couldn't walk dir to make page: {}: {}", dir.display(), e))?
            .map(|entry| {
                entry
                    .map_err(|e| af!("couldn't read dir entry in {}: {}", dir.display(), e))
                    .map(|entry| {
                        let path = entry.path();
                        (
                            path.file_name()
                                .map(|osstr| osstr.to_string_lossy().to_string()),
                            path,
                        )
                    })
            })
            .map(|filename_path| match filename_path {
                Ok((Some(filename), path)) => Ok((filename, path)),
                Ok((None, path)) => Ok((String::from("unknown"), path)),
                Err(err) => Err(err),
            })
            .collect::<Result<Vec<_>>>()?;
        paths_with_filenames.sort_by(|(f1, _), (f2, _)| f1.cmp(f2));

        for (_, path) in paths_with_filenames {
            if path == self.thumbnail_dir {
                continue;
            }

            let is_dir = path.is_dir();

            page += "<tr>";

            page += "<td";
            if let Some(thumbnail_path) = self.thumbnails.get(&path) {
                page += &format!(
                    " class=thumbnail><img src={}?thumbnail={}>",
                    config.page_root.as_ref().map(String::as_str).unwrap_or(""),
                    thumbnail_path
                        .file_name()
                        .map(OsStr::to_string_lossy)
                        .unwrap_or_else(|| Cow::Borrowed("<broken filename>"))
                );
            } else if is_dir {
                page += ">📁";
            } else {
                page += ">📃";
            }
            page += "</td>";

            page += "<td>";
            let filename = &path
                .file_name()
                .map(OsStr::to_string_lossy)
                .unwrap_or_else(|| Cow::Borrowed("<broken filename>"))
                .to_string();
            if is_dir {
                page += &format!(
                    "<a href={}/{}>{}</a>",
                    config.page_root.as_ref().map(|s| s.as_str()).unwrap_or("/"),
                    path.strip_prefix(&self.file_dir)
                        .map_err(|e| af!("couldn't strip prefix of {}: {}", path.display(), e))?
                        .display(),
                    filename
                );
            } else {
                page += filename;
            }
            page += "</td>";

            page += "</tr>\n";
        }

        page += "</table>";

        {
            let mut write = self
                .pages
                .write()
                .map_err(|e| af!("couldn't lock page cache for writing: {}", e))?;
            write.insert(dir.to_owned(), page);
        }

        let read = self
            .pages
            .read()
            .map_err(|e| af!("couldn't lock page cache for reading: {}", e))?;

        if read.contains_key(dir) {
            Ok(read.get(dir).cloned())
        } else {
            Err(af!(
                "couldn't make page for {} for some reason?",
                dir.display()
            ))
        }
    }

    fn read_config_and_make_dirs(config: &Config) -> Result<Database> {
        let thumbnail_dir = PathBuf::from(&config.thumbnail_dir);
        let thumbnail_dir = thumbnail_dir.canonicalize().map_err(|e| {
            af!(
                "couldn't create absolute thumbnail dir from {}: {}",
                thumbnail_dir.display(),
                e
            )
        })?;
        if !thumbnail_dir.exists() {
            std::fs::create_dir(&thumbnail_dir).map_err(|e| {
                af!(
                    "couldn't create thumbnail dir {}: {}",
                    thumbnail_dir.display(),
                    e
                )
            })?;
        } else if !thumbnail_dir.is_dir() {
            return Err(af!("thumbnail dir must be dir {}", thumbnail_dir.display()));
        }

        let file_dir = PathBuf::from(&config.file_dir);
        let file_dir = file_dir.canonicalize().map_err(|e| {
            af!(
                "couldn't create absolute file dir from {}: {}",
                file_dir.display(),
                e
            )
        })?;
        if !file_dir.exists() {
            std::fs::create_dir(&file_dir)
                .map_err(|e| af!("couldn't create file dir {}: {}", file_dir.display(), e))?;
        } else if !file_dir.is_dir() {
            return Err(af!("file dir must be dir {}", file_dir.display()));
        }
        if file_dir.parent().is_none() {
            return Err(af!("cannot serve files from root dir"));
        }

        let files = File::walk_dir(&file_dir, &|path| path != &thumbnail_dir)?;
        let thumbnails = build_thumbnail_db(&files, &thumbnail_dir)?;
        Ok(Database {
            file_dir,
            files,
            thumbnail_dir,
            thumbnails,
            pages: Default::default(),
        })
    }

    fn index_and_build_thumbnail_db(&self, config: &Config) -> Result<()> {
        for (file_path, thumbnail_path) in self.thumbnails.iter() {
            if !thumbnail_path.exists() || config.rebuild_thumbnails {
                tracing::info!(
                    "making thumbnail for {} -> {}",
                    file_path.display(),
                    thumbnail_path.display()
                );

                let image = match ImageReader::open(file_path)
                    .map_err(|e| {
                        af!(
                            "couldn't read file for thumbnailing: {}: {}",
                            file_path.display(),
                            e
                        )
                    })?
                    .with_guessed_format()
                    .map_err(|e| af!("couldn't guess format: {}: {}", file_path.display(), e))?
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

                thumbnail.save(thumbnail_path).map_err(|e| {
                    af!(
                        "couldn't save thumbnail for {} in {}: {}",
                        file_path.display(),
                        thumbnail_path.display(),
                        e
                    )
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Config {
    bind: String,
    auth: Option<String>,
    thumbnail_dir: String,
    file_dir: String,
    thumbnail_size: u32,
    rebuild_thumbnails: bool,
    page_root: Option<String>,
}

impl Config {
    fn read_from(config_path: &str) -> Result<Config> {
        let config_file = std::fs::read_to_string(config_path)
            .map_err(|_| af!("can't read config file {}", config_path))?;
        let toml = black_dwarf::toml::parse(&config_file)
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
                black_dwarf::toml::Value::Integer { value, .. } => (*value)
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

        Ok(Config {
            bind,
            auth,
            thumbnail_dir,
            file_dir,
            thumbnail_size,
            rebuild_thumbnails: false,
            page_root,
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
                    tracing::warn!("fucked auth header: {}", auth_value);
                    return Page::bad_request(&config);
                }
                if auth[0] != "Basic" {
                    tracing::warn!("fucked auth type: {}", auth[0]);
                    return Page::bad_request(&config);
                }
                use base64::Engine;
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&auth[1]) else {
                tracing::warn!("fucked auth: {}", auth[1]);
                return Page::bad_request(&config);
            };
                let Ok(auth) = std::str::from_utf8(&bytes) else {
                tracing::warn!("fucked auth utf8: {}", auth[1]);
                return Page::bad_request(&config);
            };
                if auth != config_auth {
                    tracing::warn!("incorrect user/pass from {}: {}", remote, auth);
                    return Page::bad_request(&config);
                }
            } else {
                return Response::text("need auth!")
                    .with_status_code(401)
                    .with_unique_header("WWW-Authenticate", "Basic realm=\"cock\"");
            }
        }

        if Some(&full_url) == config.page_root.as_ref() {
            if let Some(thumbnail) = request.get_param("thumbnail") {
                let Ok(thumb) = database.open_thumbnail(&thumbnail) else {
                    tracing::error!("couldn't read thumbnail {}", thumbnail);
                    return Page::internal_error(&config);
                };
                return Response::from_file("image/jpeg", thumb);
            }
        }

        let url = if let Some(root) = config.page_root.as_ref() {
            if !full_url.starts_with(root) {
                tracing::debug!("url didn't start with page root");
                return Page::bad_request(&config);
            }

            full_url.strip_prefix(root).unwrap()
        } else {
            &full_url
        };

        let base = PathBuf::from(&database.file_dir);
        let path = if url.is_empty() {
            base
        } else {
            base.join(&url[1..])
        };

        let path = if let Ok(path) = path.canonicalize() {
            path
        } else {
            tracing::debug!("canonicalize failed");
            path
        };

        tracing::debug!("path looks like {}", path.display());

        if path.ancestors().all(|parent| parent != &database.file_dir)
            && path
                .ancestors()
                .all(|parent| parent != &database.thumbnail_dir)
        {
            tracing::warn!(
                "preventing directory traversal: {} tried to access {}",
                remote,
                path.display()
            );
            return Page::bad_request(&config);
        }

        tracing::debug!("serving {} on \"{}\"", path.display(), url);

        if path.is_dir() {
            if let Ok(maybe_content) = database.get_content_for(&config, &path) {
                if let Some(content) = maybe_content {
                    let full_link = |path: &Path| {
                        format!(
                            "<a href={}/{}>{}</a>",
                            config.page_root.as_ref().map(|s| s.as_str()).unwrap_or(""),
                            path.strip_prefix(&database.file_dir).unwrap().display(),
                            path.display()
                        )
                    };
                    let filename_link = |path: &Path| {
                        format!(
                            "<a href={}/{}>{}</a>",
                            config.page_root.as_ref().map(|s| s.as_str()).unwrap_or(""),
                            path.strip_prefix(&database.file_dir).unwrap().display(),
                            path.file_name()
                                .map(OsStr::to_string_lossy)
                                .map(|s| s.to_string())
                                .unwrap_or("???".into())
                        )
                    };
                    let ancestors = path
                        .ancestors()
                        .take_while(|parent| *parent != database.file_dir.parent().unwrap())
                        .collect::<Vec<_>>();
                    let title = ancestors
                        .into_iter()
                        .rev()
                        .skip(1)
                        .fold(full_link(&database.file_dir), |acc, parent| {
                            acc + "/" + &filename_link(parent)
                        });

                    Page::default()
                        .with_title(title)
                        .with_content(content)
                        .render(&config)
                } else {
                    Page::not_found(&config)
                }
            } else {
                Page::internal_error(&config)
            }
        } else {
            Page::default()
                .with_title("todo")
                .with_paragraph("you're trying to download a file, this isn't implemented yet.")
                .render(&config)
        }
    });
}
