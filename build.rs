use std::{
    fs,
    io::{self, Read},
    path::Path,
    time::Duration,
};

/// How many times a download is attempted before moving to the next URL.
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// A font source: a list of URLs to try in order.
///
/// GitHub's release CDN intermittently returns 502/504, which failed the
/// v0.4.0 release build.  Each font therefore has a primary source and a
/// CDN mirror, and every download is retried before the next URL is
/// tried.  The build must not depend on a single host being healthy.
type Sources = &'static [&'static str];

const DEJAVU_SOURCES: Sources = &[
    // jsDelivr first — it is a CDN with no rate limiting and it was
    // reliable while GitHub's release CDN was returning 504.
    "https://cdn.jsdelivr.net/npm/dejavu-fonts-ttf@2.37/ttf/DejaVuSans.ttf",
    "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2",
];

const FA_SOLID_SOURCES: Sources = &[
    "https://cdn.jsdelivr.net/npm/@fortawesome/fontawesome-free@6.7.2/webfonts/fa-solid-900.ttf",
    "https://github.com/FortAwesome/Font-Awesome/releases/download/6.7.2/fontawesome-free-6.7.2-web.zip",
];

/// Download one of a font's sources, trying each URL in turn and
/// retrying each one a few times.
///
/// A source that is a bare font file is returned as-is; a `.zip` archive
/// is unwrapped by the caller.
fn download(sources: Sources, what: &str) -> Vec<u8> {
    let mut errors: Vec<String> = Vec::new();

    for url in sources {
        let is_zip = url.ends_with(".zip");
        let is_tar_bz2 = url.ends_with(".tar.bz2");

        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            match fetch(url) {
                Ok(data) if !data.is_empty() => {
                    // The archive sources are only usable if we can
                    // unwrap them; the caller does that.
                    let _ = (is_zip, is_tar_bz2);
                    return data;
                }
                Ok(_) => errors.push(format!("{url}: the response body was empty")),
                Err(e) => errors.push(format!("{url}: {e}")),
            }

            if attempt < DOWNLOAD_ATTEMPTS {
                // Back off: 2 s, then 4 s.
                let wait = Duration::from_secs(1u64 << attempt);
                println!(
                    "cargo:warning={what}: {url} failed; retrying in {}s \
                     (attempt {attempt} of {DOWNLOAD_ATTEMPTS})",
                    wait.as_secs()
                );
                std::thread::sleep(wait);
            }
        }
    }

    panic!(
        "could not download {what}. Tried:\n  {}",
        errors.join("\n  ")
    );
}

/// Fetch a URL, returning its body.
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(120))
        .call()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("failed to read the response body: {e}"))?;
    Ok(buf)
}

/// Write `data` to `dest` if it is not already there.
fn write_if_absent(dest: &Path, data: &[u8]) {
    if dest.exists() {
        return;
    }
    fs::write(dest, data).expect("failed to write the font file");
}

/// Fetch DejaVuSans.ttf.
fn fetch_dejavu(assets: &Path) {
    let dest = assets.join("DejaVuSans.ttf");
    if dest.exists() {
        return;
    }
    println!("cargo:warning=Fetching DejaVuSans.ttf...");
    let data = download(DEJAVU_SOURCES, "DejaVuSans.ttf");

    // The jsDelivr source is the font itself; the GitHub source is a
    // tarball that has to be unwrapped.
    if data.starts_with(b"\x00\x01\x00\x00")
        || data.starts_with(b"true")
        || data.starts_with(b"ttcf")
    {
        write_if_absent(&dest, &data);
        println!("cargo:warning=  -> $OUT_DIR/DejaVuSans.ttf");
        return;
    }

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

/// Fetch the Font Awesome "Solid" icon font.
fn fetch_fa_solid(assets: &Path) {
    let dest = assets.join("fa-solid.ttf");
    if dest.exists() {
        return;
    }
    println!("cargo:warning=Fetching fa-solid.ttf...");
    let data = download(FA_SOLID_SOURCES, "fa-solid.ttf");

    // The jsDelivr source is the font itself; the GitHub source is a zip
    // that has to be unwrapped.
    if data.starts_with(b"\x00\x01\x00\x00")
        || data.starts_with(b"true")
        || data.starts_with(b"ttcf")
    {
        write_if_absent(&dest, &data);
        println!("cargo:warning=  -> $OUT_DIR/fa-solid.ttf");
        return;
    }

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data.clone()))
        .expect("failed to open Font Awesome zip");
    let solid_index = (0..archive.len()).find(|&i| {
        archive.by_index(i).ok().is_some_and(|f| {
            let n = f.name().to_owned();
            n.ends_with(".ttf") && n.contains("fa-solid")
        })
    });
    match solid_index {
        Some(i) => {
            let mut file = archive.by_index(i).expect("bad zip entry");
            let mut out = fs::File::create(&dest).expect("failed to create fa-solid.ttf");
            io::copy(&mut file, &mut out).expect("failed to write fa-solid.ttf");
            println!("cargo:warning=  -> $OUT_DIR/fa-solid.ttf");
        }
        None => {
            let mut archive2 =
                zip::ZipArchive::new(std::io::Cursor::new(data)).expect("failed to reopen zip");
            let names: Vec<String> = (0..archive2.len())
                .filter_map(|i| archive2.by_index(i).ok().map(|f| f.name().to_owned()))
                .filter(|n| n.ends_with(".ttf") || n.ends_with(".otf"))
                .collect();
            panic!("Free Solid font not found in archive. Font entries present: {names:?}");
        }
    }
}

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out = Path::new(&out_dir);

    // The fonts are fetched at build time so that the binary carries them
    // and does not depend on anything being installed on the target.
    fetch_dejavu(out);
    fetch_fa_solid(out);
}
