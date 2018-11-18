use std::{env};
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = env::var("TARGET").unwrap();

    if target.contains("apple") {
        let mach_header_path = "src/platform/unix/mach/mach.h";
        println!("cargo:rerun-if-changed={}", mach_header_path);

        let mut bindings = bindgen::Builder::default()
            .header(mach_header_path)
            .derive_debug(false);
        if env::var_os("DEBUG").is_some() {
            bindings = bindings.rustfmt_bindings(true);
        }
        let bindings = bindings
            .generate()
            .expect("failed to generate bindings");

        let out_path = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        bindings
            .write_to_file(out_path.join("mach.rs"))
            .expect("failed to write bindings");
    }
}