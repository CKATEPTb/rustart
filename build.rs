mod release_version;

fn main() {
    println!("cargo:rerun-if-changed=VERSION");
    println!("cargo:rerun-if-changed=release_version.rs");
    release_version::check();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/rustart.rc");
        println!("cargo:rerun-if-changed=assets/rustart.ico");
        println!("cargo:rerun-if-changed=assets/rustart.manifest");
        println!("cargo:rerun-if-changed=assets/licenses/fluentui-system-icons.txt");
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg=/DEPENDENTLOADFLAG:0x800");
            // Load rendering, catalog and standard-launch services on first use.
            println!("cargo:rustc-link-lib=delayimp");
            for library in [
                "gdiplus.dll",
                "dwmapi.dll",
                "propsys.dll",
                "xmllite.dll",
                "wtsapi32.dll",
                "bcrypt.dll",
            ] {
                println!("cargo:rustc-link-arg=/DELAYLOAD:{library}");
            }
        }
        embed_resource::compile_for_everything(
            "assets/rustart.rc",
            release_version::resource_definitions(),
        )
        .manifest_required()
        .expect("compile Windows resources");
    }
}
