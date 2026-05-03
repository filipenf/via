use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if let Some(lib_dir) = libghostty_vt_lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    } else {
        println!("cargo:warning=libghostty-vt rpath not found");
    }
}

fn libghostty_vt_lib_dir() -> Option<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    let profile_dir = out_dir.ancestors().nth(3)?;
    let build_dir = profile_dir.join("build");

    for entry in fs::read_dir(build_dir).ok()? {
        let entry = entry.ok()?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if !file_name.starts_with("libghostty-vt-sys-") {
            continue;
        }

        let root_output = entry.path().join("root-output");
        let Ok(output_dir) = fs::read_to_string(root_output) else {
            continue;
        };
        let lib_dir = Path::new(output_dir.trim()).join("ghostty-install/lib");

        if lib_dir.exists() {
            return Some(lib_dir);
        }
    }

    None
}
