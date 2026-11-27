pub fn check() {
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        include_str!("VERSION").trim(),
        "refresh Cargo metadata with scripts/sync-version.ps1"
    );
}

pub fn resource_definitions() -> [String; 2] {
    [
        format!(
            "RUSTART_VERSION_NUMERIC={},{},{},0",
            env!("CARGO_PKG_VERSION_MAJOR"),
            env!("CARGO_PKG_VERSION_MINOR"),
            env!("CARGO_PKG_VERSION_PATCH")
        ),
        format!("RUSTART_VERSION_TEXT=\"{}\"", env!("CARGO_PKG_VERSION")),
    ]
}
