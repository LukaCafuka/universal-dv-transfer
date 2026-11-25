fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    println!("cargo:rerun-if-changed=native");
    let mut build = cc::Build::new();
    build.include("native");
    match target.as_str() {
        "windows" => {
            build
                .cpp(true)
                .file("native/windows.cpp")
                .flag_if_supported("-std=c++17")
                .flag_if_supported("/std:c++17")
                .flag_if_supported("/EHsc")
                .define("_CRT_SECURE_NO_WARNINGS", None);
            for lib in [
                "strmiids", "ole32", "oleaut32", "uuid", "user32", "setupapi", "cfgmgr32",
            ] {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        "linux" => {
            for lib in ["libraw1394", "libiec61883", "libavc1394"] {
                let info = pkg_config::Config::new().probe(lib).unwrap_or_else(|_| {
                    panic!("Install libraw1394-dev libiec61883-dev libavc1394-dev (missing {lib})")
                });
                for path in info.include_paths {
                    build.include(path);
                }
            }
            println!("cargo:rustc-link-lib=rom1394");
            build.file("native/linux.c").flag_if_supported("-std=c11");
        }
        "macos" => {
            let sdk = std::env::var("SDKROOT").ok().or_else(|| {
                std::process::Command::new("xcrun")
                    .args(["--sdk", "macosx", "--show-sdk-path"])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_owned())
            });
            let firewire=sdk.is_some_and(|p|std::path::Path::new(&p).join("System/Library/Frameworks/IOKit.framework/Headers/firewire/IOFireWireLib.h").exists());
            if firewire {
                build
                    .cpp(true)
                    .file("native/macos.cpp")
                    .flag("-std=c++17")
                    .flag("-Wno-deprecated-declarations")
                    .flag("-Wno-invalid-source-encoding")
                    .flag("-Wno-writable-strings")
                    .flag("-Wno-format-security")
                    .include("native/apple");
                for entry in std::fs::read_dir("native/apple").unwrap() {
                    let path = entry.unwrap().path();
                    if path.extension().is_some_and(|e| e == "cpp") {
                        build.file(path);
                    }
                }
                for lib in ["IOKit", "CoreFoundation", "CoreServices"] {
                    println!("cargo:rustc-link-lib=framework={lib}");
                }
            } else {
                println!(
                    "cargo:warning=This SDK has no FireWire headers. Building file workflows; build with a macOS 15 SDK for tape hardware support."
                );
                build.file("native/unavailable.c");
            }
        }
        _ => panic!("Supported operating systems: Windows, Linux, macOS"),
    }
    build.compile("udvt_native");
}
