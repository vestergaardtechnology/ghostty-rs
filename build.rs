//! Cargo build script. Replaces the parts of ghostty's Zig build
//! (`build.zig`, `src/build/*.zig`, `pkg/*/build.zig`) that link C code.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    pkg_c_libraries();
    dcimgui();
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

    // pkg/macos: the os_log_with_type macro wrapper.
    if target_os == "macos" {
        println!("cargo:rerun-if-changed=pkg/macos/os/zig_macos.c");
        cc::Build::new()
            .file("pkg/macos/os/zig_macos.c")
            .flag("-std=c99")
            .compile("ghostty_macos");
    }
    if target_os == "linux" {
        // glad's loader uses dlopen.
        println!("cargo:rustc-link-lib=dylib=dl");
    }
}

/// Dear ImGui + the Dear Bindings C API (`pkg/dcimgui/build.zig`), only with
/// the `inspector` feature. Sources are vendored in `vendor/dcimgui`.
fn dcimgui() {
    if std::env::var_os("CARGO_FEATURE_INSPECTOR").is_none() {
        return;
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if target_os == "macos" && !cfg!(target_os = "macos") {
        // Cross-checking from another host; see pkg_c_libraries.
        return;
    }
    let apple = target_vendor == "apple";

    println!("cargo:rerun-if-changed=vendor/dcimgui");
    println!("cargo:rerun-if-changed=pkg/dcimgui/ext.cpp");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .warnings(false)
        .include("vendor/dcimgui/imgui")
        .include("vendor/dcimgui/bindings")
        .define("IMGUI_HAS_DOCK", "1")
        .define("IMGUI_USE_WCHAR32", "1")
        .define("IMGUI_DISABLE_OBSOLETE_FUNCTIONS", "1")
        .define("IMGUI_ENABLE_FREETYPE", "1")
        .define("IMGUI_IMPL_API", "extern \"C\"");

    // Freetype is linked by pkg_c_libraries; only the headers are needed.
    let freetype = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("freetype2")
        .expect("pkg-config could not find freetype2");
    for path in &freetype.include_paths {
        build.include(path);
    }

    for file in [
        "imgui_demo.cpp",
        "imgui_draw.cpp",
        "imgui_tables.cpp",
        "imgui_widgets.cpp",
        "imgui.cpp",
        "misc/freetype/imgui_freetype.cpp",
    ] {
        build.file(format!("vendor/dcimgui/imgui/{file}"));
    }
    build
        .file("vendor/dcimgui/bindings/dcimgui.cpp")
        .file("vendor/dcimgui/bindings/dcimgui_internal.cpp")
        .file("pkg/dcimgui/ext.cpp");

    if apple {
        // Metal (and on macOS, OSX) backends.
        build
            .file("vendor/dcimgui/imgui/backends/imgui_impl_metal.mm")
            .flag("-fobjc-arc");
        if target_os == "macos" {
            build.file("vendor/dcimgui/imgui/backends/imgui_impl_osx.mm");
            println!("cargo:rustc-link-lib=framework=GameController");
        }
        println!("cargo:rustc-link-lib=framework=Metal");
        println!("cargo:rustc-link-lib=framework=QuartzCore");
        println!("cargo:rustc-link-lib=framework=AppKit");
    } else {
        // OpenGL3 backend should only be built on non-Apple targets.
        build
            .define("ZIGPKG_IMGUI_ENABLE_OPENGL3", "1")
            .file("vendor/dcimgui/imgui/backends/imgui_impl_opengl3.cpp");
    }

    build.compile("dcimgui");
}
