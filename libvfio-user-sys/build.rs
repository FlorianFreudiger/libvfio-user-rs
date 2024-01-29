use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Result;
use diffy::{apply, Patch};
use meson_next::config::Config;

fn patch(patch_path: &Path, target_path: &Path, keyword: &str) -> Result<()> {
    let contents = fs::read_to_string(target_path)?;

    if contents.contains(keyword) {
        // Already patched
        return Ok(());
    }

    let patch_contents = fs::read_to_string(patch_path)?;

    let patch = Patch::from_str(&*patch_contents)?;
    let contents = apply(&*contents, &patch)?;

    fs::write(target_path, contents)?;
    Ok(())
}

fn main() {
    let build_static = cfg!(feature = "build-static");
    let build_shared = cfg!(feature = "build-shared");
    let patch_dma_limit = cfg!(feature = "patch-dma-limit");

    // 1. Prepare paths
    let libvfio_user_path = PathBuf::from("libvfio-user");
    let libvfio_user_path_str = libvfio_user_path.to_str().unwrap();

    let header_path = libvfio_user_path.join("include/libvfio-user.h");
    let header_path_str = header_path.to_str().unwrap();

    let patch_path = PathBuf::from("patches/increase-dma-region-limit.patch");
    let patch_undo_path = PathBuf::from("patches/increase-dma-region-limit_undo.patch");
    let patch_target = libvfio_user_path.join("lib/private.h");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    let bindings_path = out_path.join("bindings.rs");

    let build_path = out_path.join("build");
    let build_path_str = build_path.to_str().unwrap();

    let lib_path = build_path.join("lib");
    let lib_path_str = lib_path.to_str().unwrap();

    // 2. Configure cargo
    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search={}", lib_path_str);

    // Tell cargo to tell rustc to link our `vfio-user` library. Cargo will
    // automatically know it must look for a `libvfio-user.a` or `libvfio-user.so` file.

    if build_static {
        // Prefer linking statically when both static and shared libraries are built
        // Look for a `libvfio-user.a` file
        println!("cargo:rustc-link-lib=static=vfio-user");
    } else if build_shared {
        // Look for a `libvfio-user.so` file
        println!("cargo:rustc-link-lib=dylib=vfio-user");
    } else {
        // Look for any kind of `libvfio-user` library
        println!("cargo:rustc-link-lib=vfio-user");
    }

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed={}", header_path_str);

    // 3. Build libvfio-user

    // 3.0 Dependencies
    // Try to include dependencies of libvfio-user
    // pkg_config will automatically configure cargo to link the dependencies
    if pkg_config::Config::new()
        .cargo_metadata(true)
        .atleast_version("0.11")
        .probe("json-c")
        .is_err()
    {
        println!("cargo:warning=Could not find json-c >= 0.11, build may fail");
    }

    if pkg_config::Config::new()
        .cargo_metadata(true)
        .probe("cmocka")
        .is_err()
    {
        println!("cargo:warning=Could not find cmocka, build may fail");
    }

    // 3.1 Patch libvfio-user if requested, reverse patch if not
    if patch_dma_limit {
        patch(&*patch_path, &*patch_target, "MAX_DMA_REGIONS 8192").unwrap();
    } else {
        // Ignore errors, since patch should not have any negative side effects
        let _ = patch(&*patch_undo_path, &*patch_target, "MAX_DMA_REGIONS 16");
    }

    // 3.2 Meson build
    if build_static || build_shared {
        let mut meson_options = HashMap::new();

        if build_static && build_shared {
            meson_options.insert("default_library", "both");
        } else if build_static {
            meson_options.insert("default_library", "static");
        } else {
            meson_options.insert("default_library", "shared");
        }

        let meson_config = Config::new().options(meson_options);
        meson_next::build(libvfio_user_path_str, build_path_str, meson_config);
    }

    // 3.3 Reverse patch, ignore errors
    let _ = patch(&*patch_undo_path, &*patch_target, "MAX_DMA_REGIONS 16");

    // 4. Generate bindings
    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        // The input header we would like to generate
        // bindings for.
        .header(header_path_str)
        .allowlist_file(header_path_str)
        // Parse all comments since some explanations are not doc comments (/* ... */ vs /** ... */)
        .clang_arg("-fparse-all-comments")
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    bindings
        .write_to_file(bindings_path)
        .expect("Couldn't write bindings!");
}
