use std::{env, fs, path::PathBuf};

#[path = "../release_version.rs"]
mod release_version;

fn main() {
    println!("cargo:rerun-if-changed=../VERSION");
    println!("cargo:rerun-if-changed=../release_version.rs");
    release_version::check();

    println!("cargo:rerun-if-env-changed=RUSTART_RELEASE_CAB");
    println!("cargo:rerun-if-env-changed=RUSTART_RELEASE_SHA256");
    println!("cargo:rerun-if-env-changed=RUSTART_RELEASE_SIZE");
    let cabinet =
        env::var_os("RUSTART_RELEASE_CAB").expect("build with scripts/package-release.ps1");
    println!(
        "cargo:rerun-if-changed={}",
        PathBuf::from(cabinet).display()
    );
    let hash = env::var("RUSTART_RELEASE_SHA256").expect("missing payload SHA-256");
    assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let digest: Vec<_> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hash[i..i + 2], 16).unwrap())
        .collect();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("payload.sha256"), digest).unwrap();
    let size = env::var("RUSTART_RELEASE_SIZE")
        .expect("missing payload size")
        .parse::<u64>()
        .expect("invalid payload size");
    fs::write(out.join("payload.size"), size.to_le_bytes()).unwrap();

    println!("cargo:rustc-link-arg=/DEPENDENTLOADFLAG:0x800");
    for path in [
        "package.rc",
        "../assets/rustart.manifest",
        "../assets/rustart.ico",
        "../LICENSE",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    embed_resource::compile_for_everything("package.rc", release_version::resource_definitions())
        .manifest_required()
        .expect("could not embed release package resources");
}
