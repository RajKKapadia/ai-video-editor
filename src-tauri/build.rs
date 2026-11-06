fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Whisper's Metal availability checks need Clang's compiler runtime.
        println!("cargo:rustc-link-arg=-fapple-link-rtlib");
    }

    tauri_build::build()
}
