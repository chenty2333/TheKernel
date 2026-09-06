pub fn fixtures() -> &'static std::path::Path {
    static FIXTURES: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    FIXTURES.get_or_init(|| {
        let build_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let fixtures = build_dir.join(format!("fatfs-read-{}", std::process::id()));
        let status = std::process::Command::new("sh")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/create-test-img.sh"))
            .arg(&fixtures)
            .status()
            .expect("run fixture generator (requires dosfstools and mtools)");
        assert!(status.success(), "FAT fixture generation failed");
        fixtures
    })
}
