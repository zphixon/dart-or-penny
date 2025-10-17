use std::{path::PathBuf, process::Command};

fn main() -> Result<(), ()> {
    let _ = std::fs::create_dir("frontend/modules/src/bindings");
    let mut exports: Vec<_> = std::fs::read_dir("frontend/modules/src/bindings")
        .expect("read dir")
        .filter_map(Result::ok)
        .filter_map(|p| {
            println!("cargo::rerun-if-changed={}", p.path().display());
            p.path()
                .file_stem()
                .map(std::ffi::OsStr::to_str)
                .flatten()
                .map(str::to_owned)
        })
        .filter(|f| f != "index")
        .map(|f| format!("export * from \"./{}\"", f))
        .collect();
    exports.sort();
    let mut file = std::fs::File::create("frontend/modules/src/bindings/index.ts").unwrap();
    std::io::Write::write_all(&mut file, (exports.join("\n") + "\n").as_bytes()).unwrap();

    let frontend_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("frontend")
        .join("modules");

    for ts in glob::glob(&format!(
        "{}",
        frontend_dir.join("src").join("**").join("*.tsx").display()
    ))
    .expect("glob")
    {
        let ts = ts.expect("glob item");
        println!("cargo::rerun-if-changed={}", ts.display());
    }

    let output = Command::new("cmd")
        .arg("/c")
        .arg(".\\node_modules\\.bin\\rollup.cmd -c")
        .current_dir(frontend_dir)
        .output()
        .expect("command");

    if !output.status.success() {
        eprintln!(
            "stdout: {}",
            std::str::from_utf8(&output.stdout).expect("stdout")
        );
        eprintln!(
            "stderr: {}",
            std::str::from_utf8(&output.stderr).expect("stderr")
        );
        return Err(());
    }

    Ok(())
}
