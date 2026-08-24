//! gpui_platform's `x11` feature is force-enabled by gpui-base upstream
//! (features unify across the graph), and that links `-lxkbcommon-x11`.
//! Distros often ship only the versioned `.so.0` unless the -devel package
//! is installed; point rustc at a symlink.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let candidates = [
        "/lib64/libxkbcommon-x11.so.0",
        "/usr/lib64/libxkbcommon-x11.so.0",
        "/usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0",
        "/usr/lib/libxkbcommon-x11.so.0",
    ];
    let Some(src) = candidates
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
    else {
        return;
    };
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let dest = std::path::Path::new(&out).join("libxkbcommon-x11.so");
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink(src, &dest).expect("symlink libxkbcommon-x11");
    println!("cargo:rustc-link-search=native={out}");
}
