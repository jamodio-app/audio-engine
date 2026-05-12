fn main() {
    // Crate macOS-only. Sur autre OS on génère une lib vide pour ne pas
    // casser le check workspace (le code Rust est gaté par cfg(target_os = "macos")).
    if !cfg!(target_os = "macos") {
        return;
    }

    println!("cargo:rerun-if-changed=cpp/au_host.mm");

    cc::Build::new()
        .cpp(true)
        .cpp_link_stdlib("c++")
        .file("cpp/au_host.mm")
        .flag("-fobjc-arc")
        .flag("-std=c++17")
        .flag("-Wno-deprecated-declarations")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-missing-field-initializers")
        .compile("au_host");

    for fw in [
        "AudioToolbox",
        "AudioUnit",
        "CoreAudio",
        "CoreAudioKit",
        "AVFoundation",
        "AppKit",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
