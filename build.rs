//! Cargo build script. Replaces the parts of ghostty's Zig build
//! (`build.zig`, `src/build/*.zig`, `pkg/*/build.zig`) that link C code.

// The GTK apprt's gresource XML generator (`src/apprt/gtk/build/`), shared
// with the crate.
#[allow(dead_code)]
#[path = "src/apprt/gtk/build/info.rs"]
mod info;
#[allow(dead_code, unexpected_cfgs)]
#[path = "src/apprt/gtk/build/gresource.rs"]
mod gresource;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(ghostty_gresource)");
    pkg_c_libraries();
    if std::env::var_os("CARGO_FEATURE_GTK").is_some() {
        gtk_resources();
    }
}

/// The GTK apprt's build steps (`gtkDistResources` in
/// `src/build/SharedDeps.zig`):
///
/// * exports the GTK and libadwaita versions we build against as
///   `GHOSTTY_GTK_VERSION`/`GHOSTTY_ADW_VERSION` (the `GTK_*_VERSION` and
///   `ADW_*_VERSION` header macros in Zig; see `gtk_version.rs` and
///   `adw_version.rs`), and
/// * compiles the GResource bundle (CSS, icons and the GtkBuilder `.ui`
///   files) with `glib-compile-resources` into `$OUT_DIR/ghostty.gresource`,
///   which `gresource::register` embeds. The `.ui` files are generated from
///   the Blueprints ahead of time by `tools/gen_gtk_ui.sh` and committed, so
///   the build never needs `blueprint-compiler`.
fn gtk_resources() {
    use std::path::PathBuf;
    use std::process::Command;

    for (lib, var) in [
        ("gtk4", "GHOSTTY_GTK_VERSION"),
        ("libadwaita-1", "GHOSTTY_ADW_VERSION"),
    ] {
        match pkg_config::Config::new().cargo_metadata(false).probe(lib) {
            Ok(found) => println!("cargo:rustc-env={var}={}", found.version),
            Err(err) => panic!("pkg-config could not find system library {lib}: {err}"),
        }
    }

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    for path in gresource::file_inputs() {
        println!("cargo:rerun-if-changed={path}");
    }
    for bp in gresource::BLUEPRINTS {
        println!(
            "cargo:rerun-if-changed={}/{}.{}/{}.ui",
            gresource::UI_PATH,
            bp.major,
            bp.minor,
            bp.name
        );
    }

    let xml = gresource::main(&root)
        .unwrap_or_else(|err| panic!("failed to generate the gresource XML: {err}"));
    let xml_path = out_dir.join("ghostty.gresource.xml");
    std::fs::write(&xml_path, xml).expect("failed to write the gresource XML");

    // The source root for the icons and CSS, the `.ui` directory for the
    // compiled blueprints.
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&root)
        .arg("--sourcedir")
        .arg(root.join(gresource::UI_PATH))
        .arg("--target")
        .arg(out_dir.join("ghostty.gresource"))
        .arg(&xml_path)
        .status()
        .unwrap_or_else(|err| panic!("failed to run glib-compile-resources: {err}"));
    if !status.success() {
        panic!("glib-compile-resources failed: {status}");
    }
    println!("cargo:rustc-cfg=ghostty_gresource");
}

/// C libraries wrapped by `pkg/` (freetype, harfbuzz, fontconfig, oniguruma,
/// opengl/glad). The system libraries are linked via pkg-config (the Zig
/// build's "system integration" mode); glad is always compiled statically
/// from `vendor/glad`, as `src/build/SharedDeps.zig` does.
fn pkg_c_libraries() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    for lib in ["freetype2", "harfbuzz", "fontconfig", "oniguruma"] {
        if let Err(err) = pkg_config::Config::new().probe(lib) {
            panic!("pkg-config could not find system library {lib}: {err}");
        }
    }

    // glad (OpenGL loader, always) and the glad EGL loader + libEGL
    // (non-macOS; ghostty only uses EGL with the GTK apprt).
    println!("cargo:rerun-if-changed=vendor/glad");
    let mut glad = cc::Build::new();
    glad.include("vendor/glad/include")
        .file("vendor/glad/src/gl.c")
        .warnings(false);
    if target_os != "macos" {
        glad.file("vendor/glad/src/glad_egl.c");
        if let Err(err) = pkg_config::Config::new().probe("egl") {
            panic!("pkg-config could not find system library egl: {err}");
        }
    }
    glad.compile("glad");
    if target_os == "linux" {
        // glad's loader uses dlopen.
        println!("cargo:rustc-link-lib=dylib=dl");
    }
}
