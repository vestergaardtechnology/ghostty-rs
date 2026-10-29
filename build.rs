//! Cargo build script. Replaces the parts of ghostty's Zig build
//! (`build.zig`, `src/build/*.zig`, `pkg/*/build.zig`) that link C code.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    pkg_c_libraries();
}

/// C libraries wrapped by `pkg/` (freetype, harfbuzz, fontconfig, oniguruma,
/// opengl/glad). The system libraries are linked via pkg-config (the Zig
/// build's "system integration" mode); glad is always compiled statically
/// from `vendor/glad`, as `src/build/SharedDeps.zig` does.
fn pkg_c_libraries() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Cross-checking the macOS target from another host (e.g.
    // `cargo check --target x86_64-apple-darwin` on Linux) can't compile or
    // link C code without an Apple SDK, and a check doesn't need it.
    if target_os == "macos" && !cfg!(target_os = "macos") {
        return;
    }

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
