// Adapted from xous-core/baremetal/build.rs
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    assert!(target.starts_with("riscv32"), "badgyos only builds for riscv32imac-unknown-none-elf");

    let linker_file_path = PathBuf::from("link.x");
    println!("cargo:rerun-if-changed={}", linker_file_path.display());
    println!("cargo:rustc-link-arg=-Tlink.x");

    // Put the linker script somewhere the linker can find it.
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(out_dir.join("link.x"))
        .unwrap()
        .write_all(fs::read_to_string(&linker_file_path).expect("linker file read").as_bytes())
        .unwrap();
    println!("cargo:rustc-link-search={}", out_dir.display());

    println!("cargo:rerun-if-changed=build.rs");
}
