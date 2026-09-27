use std::{env, path::PathBuf};

// Links Prism (screen reader / speech output) statically so the binary stays portable.
// Point PRISM_SDK at an unpacked prism SDK; defaults to ./prism-sdk-v0.18.2.
fn main() {
    println!("cargo:rerun-if-env-changed=PRISM_SDK");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(has_prism)");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk = env::var_os("PRISM_SDK")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("prism-sdk-v0.18.2"));
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    match os.as_str() {
        "windows" => {
            let arch_dir = if arch == "aarch64" { "arm64" } else { "x64" };
            let dir = sdk.join("windows").join(arch_dir).join("static/release/lib");
            if !dir.join("prism.lib").exists() {
                println!("cargo:warning=Prism not found at {}, screen reader announcements disabled", dir.display());
                return;
            }
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static:+whole-archive=prism");
            for lib in ["delayimp", "onecore", "uiautomationcore", "rpcrt4", "powrprof", "ole32", "oleaut32", "uuid", "shlwapi", "shell32", "advapi32", "user32"] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
            // Import libraries for optional third-party screen reader DLLs; delay-loaded so
            // the program still starts when they are absent.
            let optional = [
                ("ZDSR", "ZDSRAPI_x64.dll"),
                ("byctrl", "byctrl-x64.dll"),
                ("PCTalker", "PCTKUSR.dll"),
                ("PrismOrcaBridge", "prism_orca_bridge.dll"),
                ("PrismSpeechDispatcherBridge", "prism_speech_dispatcher_bridge.dll"),
            ];
            for (lib, dll) in optional {
                if dir.join(format!("{lib}.lib")).exists() {
                    println!("cargo:rustc-link-lib=dylib={lib}");
                    println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
                }
            }
            println!("cargo:rustc-link-arg=/DELAY:unload");
            println!("cargo:rustc-cfg=has_prism");
        }
        "macos" => {
            let dir = sdk.join("macos/universal/static/release/lib");
            if !dir.join("libprism.a").exists() {
                println!("cargo:warning=Prism not found at {}, screen reader announcements disabled", dir.display());
                return;
            }
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static:+whole-archive=prism");
            for fw in ["Foundation", "AVFoundation", "AppKit", "IOKit", "CoreFoundation"] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
            println!("cargo:rustc-link-lib=dylib=c++");
            println!("cargo:rustc-cfg=has_prism");
        }
        _ => {}
    }
}
