use anyhow::Result;
use image::{buffer::ConvertBuffer, ImageBuffer, ImageReader, Rgb};
use notify::Watcher;
use rouille::Response;
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    fs::File as FsFile,
    path::{Path, PathBuf},
    sync::RwLock,
};

mod path;

use path::{LocalPath, ServePath, ThumbnailPath};

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

#[derive(Default)]
struct Page {
    tab_title: String,
    title: String,
    content: String,
    code: Option<u16>,
}

impl Page {
    fn with_tab_title<S: ToString>(self, tab_title: S) -> Self {
        Page {
            tab_title: tab_title.to_string(),
            ..self
        }
    }

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

    fn render(self, _config: &Config) -> Response {
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
      .icon > img {{
        max-height: 3em;
        max-width: 3em;
      }}
      .filetable {{
        display: grid;
        grid-template-columns: 1fr;
        gap: 1px;
        background: green;
      }}
      .row {{
        display: grid;
        gap: 1px;
        grid-template-columns: 3em 3fr repeat(3, 1fr);
      }}
      .row > div {{
        background: white;
        padding: 0.25em;
      }}
      .row > .filename {{
        word-break: break-all;
      }}
      .icon {{
        display: flex;
        align-items: center;
        justify-content: center;
      }}
      @media (max-width: 1150px) {{
        .modified, .accessed {{
          display: none;
        }}
        .row {{
          grid-template-columns: 3em 3fr minmax(12em, 1fr);
        }}
      }}
      #searchboxdiv {{
        display: flex;
        padding-bottom: 1em;
      }}
      #searchbox {{
        flex-grow: 1;
      }}
    </style>
    <meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, minimum-scale=1, minimal-ui">
  </head>
  <body>
    <h1>{}</h1>
    <div id="searchboxdiv"><input id="searchbox" type="text" placeholder="🔎 search"/><input id="everywhere" type="checkbox"/><label for="everywhere">search everywhere?</label></div>
    {}
  </body>
</html>
"#,
            self.tab_title, title, self.content
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
    pages: RwLock<HashMap<LocalPath, String>>,
}

impl Database {
    fn open_thumbnail(&self, thumb: &str) -> Result<FsFile> {
        let thumbnail_path = self.thumbnail_dir.local_path().join(thumb);
        Ok(FsFile::open(thumbnail_path)?)
    }

    fn clear_cache(&self) -> Result<()> {
        let mut write = self
            .pages
            .write()
            .map_err(|e| af!("couldn't lock page cache for clearing: {}", e))?;
        write.clear();
        Ok(())
    }

    fn clear_cache_for(&self, path: &Path) -> Result<()> {
        let mut write = self
            .pages
            .write()
            .map_err(|e| af!("couldn't lock page cache for clearing: {}", e))?;
        let mut lp = PathBuf::from(path);
        if !lp.is_dir() {
            lp.pop();
        }
        if write.remove(&LocalPath::from(lp.clone())).is_none() {
            tracing::debug!("not cached, could not remove {}", lp.display());
        } else {
            tracing::debug!("removed {}", lp.display());
        }
        Ok(())
    }

    fn get_content_for(&self, config: &Config, serve_dir: &ServePath) -> Result<Option<String>> {
        let local_dir = LocalPath::from_serve_path(&self, config, serve_dir)?;

        {
            let read = self
                .pages
                .read()
                .map_err(|e| af!("couldn't lock page cache for reading: {}", e))?;

            if read.contains_key(&local_dir) {
                return Ok(read.get(&local_dir).cloned());
            }
        }

        let mut page = String::from("<div class=\"filetable\">");

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

        page +=
            "<div class=\"header row\"><div></div><div>filename</div><div class=\"header created\">created</div><div class=\"header modified\">modified</div><div class=\"header accessed\">accessed</div></div>\n";

        for (path, basename) in dirs.into_iter() {
            page += "<div class=\"dir row\">";

            page += "<div class=\"dir icon\">📁</div>";

            page += "<div class=\"dir filename\">";
            page += &format!(
                "<a href='{}'>{}</a>",
                ServePath::from_local_path(&self, config, &path)?.to_string(true),
                basename
            );
            page += "</div>";

            page += "<div class=\"dir created\">";
            let meta = path.local_path().metadata();
            if let Ok(created) = meta.and_then(|meta| meta.created()) {
                page += &timestamp(created);
            }
            page += "</div>";

            page += "<div class=\"dir modified\">";
            let meta = path.local_path().metadata();
            if let Ok(modified) = meta.and_then(|meta| meta.modified()) {
                page += &timestamp(modified);
            }
            page += "</div>";

            page += "<div class=\"dir accessed\">";
            let meta = path.local_path().metadata();
            if let Ok(accessed) = meta.and_then(|meta| meta.accessed()) {
                page += &timestamp(accessed);
            }
            page += "</div>";

            page += "</div>\n";
        }

        for (path, basename) in files.into_iter() {
            page += "<div class=\"file row\">";

            page += "<div class=\"file icon\"";
            if let Some(thumbnail_path) = self.thumbnails.get(&path) {
                page += &format!(
                    "><img src='{}?thumbnail={}'>",
                    config.page_root.as_ref().map(String::as_str).unwrap_or(""),
                    thumbnail_path
                        .thumbnail_path()
                        .file_name()
                        .map(OsStr::to_string_lossy)
                        .unwrap_or_else(|| Cow::Borrowed("<broken filename>"))
                );
            } else {
                page += ">📃";
            }
            page += "</div>";

            page += "<div class=\"file filename\">";
            page += &format!(
                "<a href='{}'>{}</a>",
                ServePath::from_local_path(&self, config, &path)?.to_string(true),
                basename,
            );
            page += "</div>";

            page += "<div class=\"file created\">";
            let meta = path.local_path().metadata();
            if let Ok(created) = meta.and_then(|meta| meta.created()) {
                page += &timestamp(created);
            }
            page += "</div>";

            page += "<div class=\"file modified\">";
            let meta = path.local_path().metadata();
            if let Ok(modified) = meta.and_then(|meta| meta.modified()) {
                page += &timestamp(modified);
            }
            page += "</div>";

            page += "<div class=\"file accessed\">";
            let meta = path.local_path().metadata();
            if let Ok(accessed) = meta.and_then(|meta| meta.accessed()) {
                page += &timestamp(accessed);
            }
            page += "</div>";

            page += "</div>\n";
        }

        page += &r#"</div>
<script type="text/javascript">
let sort = null;
function setSort() {
    if (sort == null) {
        sort = "mostRecentFirst";
    } else if (sort == "mostRecentFirst") {
        sort = "mostRecentLast";
    } else {
        sort = "mostRecentFirst";
    }
}

function doSort(direction, list) {
    [...list.children].sort((a, b) => {
        if (direction == "mostRecentFirst") {
            return new Date(a.children[3].innerText) < new Date(b.children[3].innerText);
        } else if (direction == "mostRecentLast") {
            return new Date(a.children[3].innerText) > new Date(b.children[3].innerText);
        } else {
            return false;
        }
    }).forEach(child => list.appendChild(child));
}

let rows = document.querySelector(".filetable");
let created = document.querySelector(".header.created");
let modified = document.querySelector(".header.modified");
let accessed = document.querySelector(".header.accessed");

created.onclick = () => { setSort(); doSort(sort, rows) };
modified.onclick = () => { setSort(); doSort(sort, rows) };
accessed.onclick = () => { setSort(); doSort(sort, rows) };

let filenames = document.querySelectorAll(".filename");
let filelist = null;
let searchbox = document.getElementById("searchbox");
let searchboxdiv = document.getElementById("searchboxdiv");
let everywhere = document.getElementById("everywhere");

function filterListForSearchbox() {
    for (searchresult of document.querySelectorAll('.everywheresearch')) {
        searchresult.remove();
    }

    if (everywhere.checked) {
        console.log(searchbox.value, filelist);
        rows.style.display = 'none';
        for (file of filelist) {
            if (file.indexOf(searchbox.value) < 0) {
                continue;
            }

            let div = document.createElement('div');
            div.classList.add('everywheresearch');

            var total = "";
            for (part of file.split('/')) {
                if (part === "") {
                    continue;
                }

                total += "/" + part;
                let a = document.createElement('a');
                a.href = total;
                a.appendChild(document.createTextNode(part));
                div.appendChild(document.createTextNode("/"));
                div.appendChild(a);
            }

            rows.parentElement.appendChild(div);
        }
    } else {
        rows.style.display = '';
        for (filename of filenames) {
            if (URL.parse(filename.childNodes[0].href).pathname.indexOf(searchbox.value) < 0) {
                filename.parentElement.style.display = 'none';
            } else {
                filename.parentElement.style.display = '';
            }
        }
    }
}

function refreshList() {
    let listurl = window.location;
    if (everywhere.checked) {
        listurl = "Easily the dumbest code I've ever written";
    }
    console.log(listurl);

    fetch(listurl + '?filelist').then(
        (response) => response.json()
    ).then(
        (json) => {
            filelist = json;
            filterListForSearchbox();
        }
    );
}

searchboxdiv.onclick = refreshList;
everywhere.onchange = refreshList;
searchbox.oninput = filterListForSearchbox;

</script>"#
            .replace(
                "Easily the dumbest code I've ever written",
                config.page_root.as_deref().unwrap_or("/"),
            );

        {
            let mut write = self
                .pages
                .write()
                .map_err(|e| af!("couldn't lock page cache for writing: {}", e))?;
            write.insert(local_dir.clone(), page);
        }

        let read = self
            .pages
            .read()
            .map_err(|e| af!("couldn't lock page cache for reading: {}", e))?;

        if read.contains_key(&local_dir) {
            Ok(read.get(&local_dir).cloned())
        } else {
            Err(af!(
                "couldn't make page for {} for some reason?",
                local_dir.local_path().display()
            ))
        }
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
            pages: Default::default(),
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
    cache_clear_interval: u64,
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
                    .ok_or_else(|| {
                        af!("page_root must be a string in config file {}", config_path)
                    })
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

        let cache_clear_interval = toml
            .get("cache_clear_interval")
            .iter()
            .flat_map(|cci| cci.as_integer())
            .next()
            .unwrap_or(60 * 60) as u64;

        Ok(Config {
            bind,
            auth,
            thumbnail_dir,
            file_dir,
            thumbnail_size,
            rebuild_thumbnails: false,
            page_root,
            auth_realm,
            cache_clear_interval,
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

    let file_dir = config.file_dir.clone();
    let thumb_dir = Box::leak(Box::new(db.thumbnail_dir.clone()));
    std::thread::spawn(move || {
        let mut watcher = notify::recommended_watcher(|r: Result<notify::Event, _>| match r {
            Ok(ev) => {
                for path in ev
                    .paths
                    .iter()
                    .filter(|path| !path.starts_with(thumb_dir.local_path()))
                {
                    tracing::info!("clearing cache for {}, got fs update", path.display());
                    if db.clear_cache_for(path).is_err() {
                        tracing::error!("could not clear cache");
                    }
                }
            }
            _ => {}
        })
        .expect("could not create fs watcher");

        watcher
            .watch(Path::new(&file_dir), notify::RecursiveMode::Recursive)
            .expect("could not watch file dir");

        loop {
            std::thread::sleep(std::time::Duration::from_secs(config.cache_clear_interval));
            db.clear_cache().expect("could not clear entire cache");
        }
    });

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
                    return Page::bad_request(&config);
                }
                if auth[0] != "Basic" {
                    tracing::warn!("broken auth type: {}", auth[0]);
                    return Page::bad_request(&config);
                }
                use base64::Engine;
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&auth[1]) else {
                    tracing::warn!("broken auth: {}", auth[1]);
                    return Page::bad_request(&config);
                };
                let Ok(auth) = std::str::from_utf8(&bytes) else {
                    tracing::warn!("broken auth utf8: {}", auth[1]);
                    return Page::bad_request(&config);
                };
                if auth != config_auth {
                    tracing::warn!("incorrect user/pass from {}: {}", remote, auth);
                    return Page::bad_request(&config);
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
                    return Page::internal_error(&config);
                };
                return Response::from_file("image/jpeg", thumb)
                    .with_unique_header("Cache-Control", "public, max-age=604800, immutable");
            }
        }

        let url = if let Some(root) = config.page_root.as_ref() {
            if !full_url.starts_with(root) {
                tracing::debug!("url didn't start with page root");
                return Page::bad_request(&config);
            }

            &full_url
        } else {
            &full_url
        };

        let url_serve_path = ServePath::from(PathBuf::from(url));
        let Ok(request_local_path) = LocalPath::from_serve_path(&db, &config, &url_serve_path)
        else {
            return Page::bad_request(&config);
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
            return Page::bad_request(&config);
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

            if let Ok(maybe_content) = db.get_content_for(&config, &url_serve_path) {
                if let Some(content) = maybe_content {
                    let full_link = |path: &LocalPath| -> Result<String> {
                        Ok(format!(
                            "<a href='{}'>{}</a>",
                            ServePath::from_local_path(&db, &config, path)?.to_string(true),
                            path.local_path().display(),
                        ))
                    };
                    let filename_link = |path: &LocalPath| -> Result<String> {
                        Ok(format!(
                            "<a href='{}'>{}</a>",
                            ServePath::from_local_path(&db, &config, path)?.to_string(true),
                            path.local_path()
                                .file_name()
                                .map(OsStr::to_string_lossy)
                                .map(|s| s.to_string())
                                .unwrap_or("???".into())
                        ))
                    };
                    let ancestors = request_local_path
                        .local_path()
                        .ancestors()
                        .take_while(|parent| *parent != db.file_dir.local_path().parent().unwrap())
                        .collect::<Vec<_>>();
                    let Ok(title) = ancestors.into_iter().rev().skip(1).fold(
                        full_link(&db.file_dir),
                        |acc, parent| {
                            acc.and_then(|acc| {
                                let link = filename_link(&LocalPath::from(parent.to_path_buf()))?;
                                Ok(acc + "/" + &link)
                            })
                        },
                    ) else {
                        return Page::internal_error(&config);
                    };

                    Page::default()
                        .with_tab_title(request_local_path.local_path().display())
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
            let Ok(file) = std::fs::File::open(request_local_path.local_path()) else {
                return Page::not_found(&config);
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
