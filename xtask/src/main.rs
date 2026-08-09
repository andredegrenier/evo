//! Dev-time tasks. `cargo run -p xtask -- icons` regenerates all icon
//! artifacts from assets/icon/evo.svg; `cargo run -p xtask -- fidelity`
//! measures how evo draws a corpus of PDFs. Never a dependency of the app.

use std::path::{Path, PathBuf};

mod fidelity;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or_default();
    match task {
        "icons" => icons(),
        "fidelity" => fidelity::main(&args[1..]),
        _ => {
            eprintln!("usage: cargo run -p xtask -- <icons|fidelity>");
            eprintln!();
            eprintln!("{}", fidelity::USAGE);
            std::process::exit(1);
        }
    }
}

/// The repository, found from where this crate is rather than from where it
/// was run: `cargo run -p xtask` keeps the caller's working directory, and the
/// harness has to find the same corpus manifests either way.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ has a parent")
        .to_path_buf()
}

fn render_png(tree: &resvg::usvg::Tree, size: u32) -> resvg::tiny_skia::Pixmap {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// tiny-skia pixmaps are premultiplied; ico/icns want straight RGBA.
fn unpremultiplied(pixmap: &resvg::tiny_skia::Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn icons() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let icon_dir = root.join("assets/icon");
    let svg = std::fs::read(icon_dir.join("evo.svg")).expect("read evo.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
        .expect("parse evo.svg");

    // PNGs used at runtime and in docs.
    for size in [1024u32, 256] {
        let pixmap = render_png(&tree, size);
        pixmap
            .save_png(icon_dir.join(format!("evo-{size}.png")))
            .expect("write png");
        println!("wrote evo-{size}.png");
    }

    // Windows .ico: multiple sizes in one file.
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 32, 48, 64, 128, 256] {
        let pixmap = render_png(&tree, size);
        let image = ico::IconImage::from_rgba_data(size, size, unpremultiplied(&pixmap));
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ico"));
    }
    let mut f = std::fs::File::create(icon_dir.join("evo.ico")).unwrap();
    dir.write(&mut f).expect("write ico");
    println!("wrote evo.ico");

    // macOS .icns.
    let mut family = icns::IconFamily::new();
    for size in [16u32, 32, 64, 128, 256, 512, 1024] {
        let pixmap = render_png(&tree, size);
        let image = icns::Image::from_data(
            icns::PixelFormat::RGBA,
            size,
            size,
            unpremultiplied(&pixmap),
        )
        .expect("icns image");
        // Not every size has an icns type; skip those that don't.
        if family.add_icon(&image).is_err() {
            println!("  (skipped {size}px, no icns slot)");
        }
    }
    let mut f = std::fs::File::create(icon_dir.join("evo.icns")).unwrap();
    family.write(&mut f).expect("write icns");
    println!("wrote evo.icns");
}
