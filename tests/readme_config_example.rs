// Guard: the TOML example in README.md ("完整示例（生产 canary 18 键）") is
// the file operators copy — it must parse and validate through the real
// `config::load` at all times. A doc drift that would brick a deployment
// fails this test instead of a customer's container.
#[test]
fn readme_config_example_loads_and_validates() {
    let readme = include_str!("../README.md");
    let start = readme
        .find("```toml")
        .expect("README must contain a ```toml block");
    let body = &readme[start + "```toml".len()..];
    let end = body.find("```").expect("the toml block must be closed");
    let text = &body[..end];

    // Anchor: the example, not some other future block.
    assert!(
        text.contains("version = 1"),
        "the first ```toml block must be the config example"
    );

    let path = std::env::temp_dir().join(format!("atg-readme-example-{}.toml", std::process::id()));
    std::fs::write(&path, text).expect("write README example");

    let loaded = agent_trace_gateway::config::load(&path)
        .unwrap_or_else(|e| panic!("README config example must load+validate: {e}"));
    // The 18-key canary shape, spot-checked.
    assert_eq!(loaded.config.version, 1);
    assert_eq!(loaded.config.server.listen, "0.0.0.0:6180");
    assert_eq!(loaded.config.server.upstream, "http://sub2api:8080");
    assert_eq!(loaded.config.capture.max_bytes, 16777216);
    assert_eq!(loaded.config.export.workers, 4);
    assert!(!loaded.config.api_key.salt().is_empty());
    let _ = std::fs::remove_file(&path);
}
