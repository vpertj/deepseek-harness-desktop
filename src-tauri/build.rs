fn main() {
    // Force the build script (and the embedded frontend assets) to rebuild
    // whenever the frontend output changes. Without this, cargo reuses a
    // stale OUT_DIR and the release binary embeds an old (or empty) web UI.
    println!("cargo:rerun-if-changed=../build");
    tauri_build::build()
}
