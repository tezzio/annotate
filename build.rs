use std::{
    fs,
    io::{self, Read},
    path::Path,
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

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out = Path::new(&out_dir);

    fetch_dejavu(out);
    fetch_fa_solid(out);
}
