//! Compiles the macOS drag-ghost overlay helper (tools/drag-ghost.swift)
//! into OUT_DIR so the sidebar binary can embed it. Elsewhere, or without
//! `swiftc`, an empty file is embedded and the feature stays off.
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dest = out.join("drag-ghost");
    println!("cargo:rerun-if-changed=tools/drag-ghost.swift");
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        // Match the Rust target (release CI cross-builds x86_64 on arm64
        // runners) and a conservative minimum OS, or the helper is a
        // different architecture / refuses to load on older macOS.
        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("aarch64") => "arm64",
            Ok("x86_64") => "x86_64",
            _ => "arm64",
        };
        let minos = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
        println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
        let status = std::process::Command::new("swiftc")
            .args(["-O", "-target", &format!("{arch}-apple-macosx{minos}"), "-o"])
            .arg(&dest)
            .arg("tools/drag-ghost.swift")
            .status();
        match status {
            Ok(s) if s.success() => return,
            Ok(s) => println!("cargo:warning=drag-ghost: swiftc failed ({s}); overlay disabled"),
            Err(e) => println!("cargo:warning=drag-ghost: swiftc unavailable ({e}); overlay disabled"),
        }
    }
    std::fs::write(&dest, []).expect("write empty drag-ghost stub");
}
