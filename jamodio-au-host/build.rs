fn main() {
    // Crate macOS-only. Sur autre OS on génère une lib vide pour ne pas
    // casser le check workspace (le code Rust est gaté par cfg(target_os = "macos")).
    if !cfg!(target_os = "macos") {
        return;
    }

    println!("cargo:rerun-if-changed=cpp/au_host.mm");
    println!("cargo:rerun-if-changed=cpp/audio_workgroup.mm");

    cc::Build::new()
        .cpp(true)
        .cpp_link_stdlib("c++")
        .file("cpp/au_host.mm")
        // Sprint S2 — bindings CoreAudio Workgroup (os_workgroup_join). Compilé
        // dans la même static lib `au_host` que le hosting AU pour éviter un
        // crate satellite. Le code est macOS-only (gardes @available 11.0).
        .file("cpp/audio_workgroup.mm")
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
        "CoreMIDI",          // S2 — MIDIEventListInit/Add pour les AU v3
        "AVFoundation",
        "AppKit",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
