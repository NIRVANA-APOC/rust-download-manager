//! Windows-GNU manifest work-around for GPUI.
//!
//! GPUI imports `TaskDialogIndirect`, which only Common-Controls **v6** exports.
//! A process only binds v6 when its executable carries an application manifest
//! that declares the dependency. GPUI ships that manifest behind its
//! `windows-manifest` feature (embedded via `embed-resource`/`windres`).
//!
//! On the `x86_64-pc-windows-gnu` target the MinGW GCC driver unconditionally
//! links its own `default-manifest.o` (see `gcc -dumpspecs`), so GNU ld ends up
//! with two `.rsrc` manifests and reports:
//!
//! ```text
//! ld: .rsrc merge failure: multiple non-default manifests
//! ```
//!
//! and keeps the *default* (MinGW) manifest — which does not declare
//! Common-Controls v6. The loader then binds `comctl32.dll` v5.82 and the
//! process dies at startup with `STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139)`
//! before `main` runs.
//!
//! Fix: put an empty `default-manifest.o` in a shadow directory and add it to
//! GCC's start-file search path through `-B`. GCC's `%:if-exists(...)` spec
//! pick then resolves to our empty object, the MinGW default manifest is not
//! linked, and GPUI's Common-Controls v6 manifest is the only `.rsrc` resource.
//!
//! This is a no-op for non-Windows and MSVC targets (MSVC's linker merges the
//! manifest correctly and the feature works out of the box there).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "gnu" {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    embed_app_icon(&out_dir);
    shadow_mingw_default_manifest(&out_dir);
}

/// Embed `assets/branding/confluence/rdm.ico` as the executable's application icon.
///
/// Windows shows the executable's icon in Explorer/taskbar; GPUI 0.2.2 has no
/// per-window icon API, so this is the only way to brand the desktop app.
///
/// The icon is compiled with `windres` into a separate object file that only
/// contributes a `.rsrc` icon directory entry — it is **not** a manifest, so it
/// merges cleanly with GPUI's Common-Controls v6 manifest resource. A missing
/// `windres` (e.g. stripped-down cross toolchain) only warns: the GUI still
/// builds, just without the custom icon.
fn embed_app_icon(out_dir: &std::path::Path) {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.clone());
    let icon = repo_root.join("assets/branding/confluence/rdm.ico");
    if !icon.is_file() {
        println!(
            "cargo:warning=rdm-gui: app icon not found at {} (building without icon)",
            icon.display()
        );
        return;
    }
    println!("cargo:rerun-if-changed={}", icon.display());

    let rc_path = out_dir.join("rdm_app_icon.rc");
    // windres wants escaped backslashes inside the quoted path.
    let icon_escaped = icon.display().to_string().replace('\\', "\\\\");
    if let Err(error) = fs::write(&rc_path, format!("IDI_RDM_APP ICON \"{icon_escaped}\"\n")) {
        println!("cargo:warning=rdm-gui: could not write icon resource script: {error}");
        return;
    }
    let object = out_dir.join("rdm_app_icon.o");
    let windres = env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
    match Command::new(&windres)
        .arg("-O")
        .arg("coff")
        .arg("-i")
        .arg(&rc_path)
        .arg("-o")
        .arg(&object)
        .output()
    {
        Ok(output) if output.status.success() => {
            println!("cargo:rustc-link-arg={}", object.display());
        }
        Ok(output) => println!(
            "cargo:warning=rdm-gui: windres failed, building without app icon: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => println!(
            "cargo:warning=rdm-gui: could not run '{windres}' ({error}); building without app icon"
        ),
    }
}

/// Put an empty `default-manifest.o` in a shadow directory and add it to GCC's
/// start-file search path through `-B`, so GCC's `%:if-exists(...)` spec pick
/// resolves to our empty object instead of MinGW's default manifest. See the
/// module docs for the full `STATUS_ENTRYPOINT_NOT_FOUND` root cause.
fn shadow_mingw_default_manifest(out_dir: &std::path::Path) {
    let shadow = out_dir.join("manifest-shadow");
    fs::create_dir_all(&shadow).expect("create manifest shadow dir");

    let source = out_dir.join("rdm_empty_default_manifest.c");
    fs::write(
        &source,
        "/* Intentionally empty: replaces MinGW's default-manifest.o so that\n \
         * GPUI's Common-Controls v6 manifest is the only embedded manifest. */\n \
         int rdm_default_manifest_placeholder = 0;\n",
    )
    .expect("write placeholder source");

    let object = shadow.join("default-manifest.o");
    let compiler = env::var("CC").unwrap_or_else(|_| "gcc".to_string());
    let output = Command::new(&compiler)
        .arg("-c")
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to run '{compiler}' while building rdm-gui: {e}. \
                 A working MinGW GCC is required for the windows-gnu target."
            )
        });
    if !output.status.success() {
        panic!(
            "failed to compile the empty default-manifest object with '{compiler}':\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // `-B` must reach the linker driver (GCC), not GNU ld itself.
    println!("cargo:rustc-link-arg=-B{}", shadow.display());
}
