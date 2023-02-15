use anyhow::{anyhow, Result};
use image::io::Reader as ImageReader;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

macro_rules! af {
    ($($tt:tt)*) => { {
        let msg = format!($($tt)*);
        tracing::error!("{}:{} {}", file!(), line!(), msg);
        anyhow!(msg)
    } };
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

fn build_thumbnail_db(
    files: &[File],
    thumbnail_dir: &Path,
) -> Result<HashMap<PathBuf, Option<PathBuf>>> {
    fn btdb_rec(
        db: &mut HashMap<PathBuf, Option<PathBuf>>,
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
                    db.insert(path, Some(thumbnail_path));
                }
                File::File(path) => {
                    let path = path.canonicalize().map_err(|e| {
                        af!("couldn't get absolute path for {}: {}", path.display(), e)
                    })?;
                    db.insert(path, None);
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
struct Config {
    files: Vec<File>,
    file_dir: PathBuf,
    thumbnails: HashMap<PathBuf, Option<PathBuf>>,
    thumbnail_dir: PathBuf,
    thumbnail_size: u32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = std::env::args().collect::<Vec<_>>();
    let config = read_config_and_make_dirs(
        args.get(1)
            .ok_or_else(|| af!("need config file argument"))?,
    )?;

    tracing::debug!("{:#?}", config);

    tracing::info!("checking thumbnail database");
    let rebuild_thumbnails = Some("--rebuild-thumbnails") == args.get(2).map(|s| &**s);
    for (file_path, thumbnail_path) in config.thumbnails.iter() {
        if let Some(thumbnail_path) = thumbnail_path {
            if !thumbnail_path.exists() || rebuild_thumbnails {
                tracing::info!(
                    "making thumbnail for {} -> {}",
                    file_path.display(),
                    thumbnail_path.display()
                );

                let image = ImageReader::open(file_path)
                    .map_err(|e| {
                        af!(
                            "couldn't read file for thumbnailing: {}: {}",
                            file_path.display(),
                            e
                        )
                    })?
                    .decode()
                    .map_err(|e| af!("couldn't decode file: {}: {}", file_path.display(), e))?;

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
    }

    Ok(())
}

fn read_config_and_make_dirs(config_path: &str) -> Result<Config> {
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
        })?;
    let thumbnail_dir = PathBuf::from(thumbnail_dir);
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

    let file_dir = toml
        .get("file_dir")
        .ok_or_else(|| af!("need file dir in config file {}", config_path))?
        .as_str()
        .ok_or_else(|| af!("file dir must be string in config file {}", config_path))?;
    let file_dir = PathBuf::from(file_dir);
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

    let files = File::walk_dir(&file_dir, &|path| path != &thumbnail_dir)?;
    let thumbnails = build_thumbnail_db(&files, &thumbnail_dir)?;
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

    Ok(Config {
        thumbnails,
        thumbnail_dir,
        thumbnail_size,
        file_dir,
        files,
    })
}
