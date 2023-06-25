use std::env;
use std::path::PathBuf;

use meson_next::config::Config;

fn main() {
    // 1. Prepare paths
    let libvfio_user_path = PathBuf::from("libvfio-user");
    let libvfio_user_path_str = libvfio_user_path.to_str().unwrap();

    let header_path = libvfio_user_path.join("include/libvfio-user.h");
    let header_path_str = header_path.to_str().unwrap();

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
    // automatically know it must look for a `libvfio-user.so` file.
    println!("cargo:rustc-link-lib=vfio-user");

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed={}", header_path_str);

    // 3. Build libvfio-user
    meson_next::build(libvfio_user_path_str, build_path_str, Config::new());

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
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    bindings
        .write_to_file(bindings_path)
        .expect("Couldn't write bindings!");
}
