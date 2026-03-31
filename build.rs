use std::{
    fs,
    io::{self, BufRead, Read},
    path::Path,
    thread,
};

fn download(url: &str) -> Vec<u8> {
    let resp = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("HTTP request failed for {url}: {e}"));
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .expect("failed to read HTTP response body");
    buf
}

fn fetch_dejavu(assets: &Path) {
    let dest = assets.join("DejaVuSans.ttf");
    if dest.exists() {
        return;
    }
    println!("cargo:warning=Fetching DejaVuSans.ttf...");
    let data = download(
        "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/\
         version_2_37/dejavu-fonts-ttf-2.37.tar.bz2",
    );
    let decoder = bzip2::read::BzDecoder::new(io::Cursor::new(data));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().expect("failed to iterate tar entries") {
        let mut entry = entry.expect("bad tar entry");
        let path = entry.path().expect("bad tar path").into_owned();
        if path.ends_with("DejaVuSans.ttf") {
            let mut out = fs::File::create(&dest).expect("failed to create DejaVuSans.ttf");
            io::copy(&mut entry, &mut out).expect("failed to write DejaVuSans.ttf");
            println!("cargo:warning=  -> $OUT_DIR/DejaVuSans.ttf");
            return;
        }
    }
    panic!("DejaVuSans.ttf not found inside the downloaded archive");
}

fn fetch_fa_solid(assets: &Path) {
    let dest = assets.join("fa-solid.otf");
    if dest.exists() {
        return;
    }
    println!("cargo:warning=Fetching fa-solid.otf...");
    let data = download(
        "https://github.com/FortAwesome/Font-Awesome/releases/download/\
         6.7.2/fontawesome-free-6.7.2-desktop.zip",
    );
    // Clone so we can reuse the bytes for diagnostics if the entry isn't found.
    let mut archive =
        zip::ZipArchive::new(io::Cursor::new(data.clone())).expect("failed to open Font Awesome zip");
    let solid_index = (0..archive.len()).find(|&i| {
        archive.by_index(i).ok().map_or(false, |f| {
            let n = f.name().to_owned();
            // The exact filename varies across releases (space vs dash before "Solid").
            n.ends_with(".otf") && n.contains("Free") && n.contains("Solid")
        })
    });
    match solid_index {
        Some(i) => {
            let mut file = archive.by_index(i).expect("bad zip entry");
            let mut out = fs::File::create(&dest).expect("failed to create fa-solid.otf");
            io::copy(&mut file, &mut out).expect("failed to write fa-solid.otf");
            println!("cargo:warning=  -> $OUT_DIR/fa-solid.otf");
        }
        None => {
            // Collect OTF names for a useful diagnostic if the layout ever changes.
            let mut archive2 =
                zip::ZipArchive::new(io::Cursor::new(data)).expect("failed to reopen zip");
            let names: Vec<String> = (0..archive2.len())
                .filter_map(|i| archive2.by_index(i).ok().map(|f| f.name().to_owned()))
                .filter(|n| n.ends_with(".otf"))
                .collect();
            panic!("Free Solid OTF not found in archive. OTF entries present: {names:?}");
        }
    }
}

/// Compile the annotator crate as a release binary into `target_dir` using a
/// child cargo process.  Using a private `--target-dir` avoids conflicting with
/// the outer cargo's file lock.  `ANNOTATOR_INNER_BUILD=1` stops the nested
/// build.rs from re-entering `build_iso()`.
fn cargo_release_build(cargo: &str, manifest_dir: &Path, target_dir: &Path, fail_msg: &str) {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(cargo);
    cmd.args([
            "build", "--release", "--locked",
            "--manifest-path", &format!("{}/Cargo.toml", manifest_dir.display()),
            "--target-dir",    target_dir.to_str().unwrap(),
        ])
        .env("ANNOTATOR_INNER_BUILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("cargo not found");
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();
    let t_out = thread::spawn(move || {
        io::BufReader::new(child_stdout)
            .lines()
            .for_each(|l| eprintln!("{}", l.unwrap_or_default()));
    });
    let t_err = thread::spawn(move || {
        io::BufReader::new(child_stderr)
            .lines()
            .for_each(|l| eprintln!("{}", l.unwrap_or_default()));
    });
    let status = child.wait().expect("failed to wait on cargo");
    t_out.join().unwrap();
    t_err.join().unwrap();
    assert!(status.success(), "{fail_msg}");
}

/// Runs `make` with the given args, streaming stdout+stderr to the terminal in
/// real-time via the build script's stderr (which cargo forwards to the
/// terminal).  Pass `Some(("VAR", ""))` to have an env var removed before
/// the child is spawned ("" value is ignored; the var is removed).
fn run_make_streaming(args: &[&str], remove_env: &Option<(&str, &str)>, fail_msg: &str) {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("make");
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((var, _)) = remove_env {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().expect("make not found");

    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    // Forward stdout → our stderr so cargo passes it to the terminal.
    let t_out = thread::spawn(move || {
        io::BufReader::new(child_stdout)
            .lines()
            .for_each(|l| eprintln!("{}", l.unwrap_or_default()));
    });
    let t_err = thread::spawn(move || {
        io::BufReader::new(child_stderr)
            .lines()
            .for_each(|l| eprintln!("{}", l.unwrap_or_default()));
    });

    let status = child.wait().expect("failed to wait on make");
    t_out.join().unwrap();
    t_err.join().unwrap();
    assert!(status.success(), "{fail_msg}");
}

fn build_iso(out: &Path, manifest_dir: &Path) {
    use std::process::Command;

    let br_dir = out.join("buildroot");
    let br_out = out.join("buildroot-output");
    let external = manifest_dir.join("buildroot");

    // ── Clone buildroot if not already present ────────────────────────────────
    if !br_dir.join("Makefile").exists() {
        println!("cargo:warning=Cloning buildroot (depth 1)...");
        let status = Command::new("git")
            .args([
                "clone", "--depth=1",
                "https://github.com/buildroot/buildroot.git",
                br_dir.to_str().unwrap(),
            ])
            .status()
            .expect("git not found — required to clone buildroot");
        assert!(status.success(), "git clone buildroot failed");
    }

    fs::create_dir_all(&br_out).expect("failed to create buildroot output dir");

    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);

    // ── Pre-build the annotator release binary ────────────────────────────────
    // We compile it here (before invoking buildroot) using a private target
    // directory so there is no conflict with the outer cargo's target lock.
    // Setting ANNOTATOR_INNER_BUILD=1 prevents this nested cargo from entering
    // build_iso() again, which would cause infinite recursion.
    let bin_target_dir = out.join("annotator-bin");
    fs::create_dir_all(&bin_target_dir).expect("failed to create annotator-bin dir");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!("cargo:warning=Compiling annotator release binary...");
    cargo_release_build(
        &cargo,
        manifest_dir,
        &bin_target_dir,
        "annotator release binary build failed",
    );
    let prebuilt_bin = bin_target_dir.join("release/annotator");
    assert!(prebuilt_bin.exists(), "release binary not found at {}", prebuilt_bin.display());

    // ── make defconfig ────────────────────────────────────────────────────────
    println!("cargo:warning=Configuring buildroot (annotator_defconfig)...");
    run_make_streaming(
        &[
            "-C", br_dir.to_str().unwrap(),
            &format!("O={}", br_out.display()),
            &format!("BR2_EXTERNAL={}", external.display()),
            "annotator_defconfig",
        ],
        &None,
        "make annotator_defconfig failed",
    );

    // ── make ──────────────────────────────────────────────────────────────────
    // ANNOTATOR_OVERRIDE_SRCDIR tells buildroot where the source lives.
    // ANNOTATOR_PREBUILT_BIN passes the already-compiled binary so the
    // annotator package skips cargo entirely and just installs the file.
    println!("cargo:warning=Building buildroot ISO (this takes a while)...");
    run_make_streaming(
        &[
            "-C", br_dir.to_str().unwrap(),
            &format!("O={}", br_out.display()),
            &format!("-j{nproc}"),
            &format!("ANNOTATOR_OVERRIDE_SRCDIR={}", manifest_dir.display()),
            &format!("ANNOTATOR_PREBUILT_BIN={}", prebuilt_bin.display()),
        ],
        &None,
        "buildroot make failed",
    );

    let iso = br_out.join("images/rootfs.iso9660");
    println!("cargo:warning=ISO ready: {}", iso.display());
}

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = Path::new(&out_dir);

    fetch_dejavu(out);
    fetch_fa_solid(out);

    // ANNOTATOR_INNER_BUILD is set by the buildroot annotator package's
    // ANNOTATOR_BUILD_CMDS before invoking cargo.  When set we are running
    // inside buildroot and must NOT re-enter build_iso() — doing so would
    // start another buildroot instance and cause infinite recursion.
    //
    // As a belt-and-suspenders guard, also detect the inner build structurally:
    // when cargo is invoked by buildroot, OUT_DIR lives underneath
    // "buildroot-output/" in the outer build's output tree.  This check works
    // even if the env var is stripped by $(TARGET_MAKE_ENV) or a wrapper script.
    println!("cargo:rerun-if-env-changed=ANNOTATOR_INNER_BUILD");
    let inner_by_env = std::env::var("ANNOTATOR_INNER_BUILD").is_ok();
    let inner_by_path = out.components().any(|c| c.as_os_str() == "buildroot-output");
    if !inner_by_env && !inner_by_path {
        build_iso(out, Path::new(&manifest_dir));
    }
}
