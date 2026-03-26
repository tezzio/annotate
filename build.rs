use std::{path::Path, process::Command};

fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dejavu = Path::new(&root).join("assets/DejaVuSans.ttf");
    let fa     = Path::new(&root).join("assets/fa-solid.otf");

    if !dejavu.exists() || !fa.exists() {
        eprintln!("Assets missing — running fetch-assets.sh ...");
        let status = Command::new("bash")
            .arg(Path::new(&root).join("fetch-assets.sh"))
            .status()
            .expect("failed to run fetch-assets.sh");
        if !status.success() {
            panic!("fetch-assets.sh failed — run it manually and retry.");
        }
    }

    println!("cargo:rerun-if-changed=assets/DejaVuSans.ttf");
    println!("cargo:rerun-if-changed=assets/fa-solid.otf");
}
