fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-examples=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
    }

    tauri_build::build();
    globalize_xcode27_swift_rs_exports();
}

/// Xcode 27's SwiftPM emits the SwiftRs dependency object inside Tauri's
/// static archive with local (`t`) C symbols. `swift-rs` 1.0.9 correctly
/// globalizes the package's own exports, but intentionally leaves dependency
/// members alone to avoid duplicate symbols across plugin archives. Tauri's
/// Rust side also calls the three SwiftRs bridge functions, so make only that
/// dependency member's bridge exports global after all dependency build
/// scripts have produced the archive.
///
/// This runs from the application build script (after Tauri's build script),
/// keeps the workaround in version control, and is a no-op for desktop and
/// Android targets. It avoids hand-editing generated archives.
fn globalize_xcode27_swift_rs_exports() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("ios") {
        return;
    }

    let Some(out_dir) = std::env::var_os("OUT_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    let Some(build_dir) = out_dir.parent().and_then(|path| path.parent()) else {
        return;
    };
    let Some(objcopy) = rustup_llvm_objcopy() else {
        println!(
            "cargo:warning=iOS build needs rustup component llvm-tools-preview for Xcode 27 Swift exports"
        );
        return;
    };

    let mut archives = Vec::new();
    collect_tauri_archives(build_dir, 0, &mut archives);
    for archive in archives {
        let status = std::process::Command::new(&objcopy)
            .args([
                "--globalize-symbol=_retain_object",
                "--globalize-symbol=_release_object",
                "--globalize-symbol=_data_from_bytes",
                "--globalize-symbol=_string_from_bytes",
            ])
            .arg(&archive)
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            println!(
                "cargo:warning=failed to globalize SwiftRs exports in {}",
                archive.display()
            );
            continue;
        }
        // objcopy changes the members in-place. Rebuild the archive index so
        // Apple's ld sees the newly globalized symbols when it searches a
        // static plugin library (the Xcode 27 linker otherwise keeps the
        // stale SwiftPM symbol table).
        let _ = std::process::Command::new("xcrun")
            .args(["ranlib"])
            .arg(&archive)
            .status();
    }
}

fn collect_tauri_archives(
    path: &std::path::Path,
    depth: usize,
    archives: &mut Vec<std::path::PathBuf>,
) {
    if depth > 10 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_tauri_archives(&path, depth + 1, archives);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("a")
            && path.to_string_lossy().contains("/swift-rs/")
        {
            archives.push(path);
        }
    }
}

fn rustup_llvm_objcopy() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let host = format!("{}-apple-darwin", std::env::consts::ARCH);
    let path = std::path::Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin/llvm-objcopy");
    path.is_file().then_some(path)
}
