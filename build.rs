fn main() {
    println!("cargo:rustc-check-cfg=cfg(mnest_legacy_avio_write)");
    if let Ok(library) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavformat")
    {
        let major = library
            .version
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok());
        if major.is_some_and(|major| major < 61) {
            println!("cargo:rustc-cfg=mnest_legacy_avio_write");
        }
    }
}
