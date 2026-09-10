use cfg_lib::conf::{try_init_cfg, ConfigError};

#[test]
fn repeated_initialization_returns_typed_error() {
    try_init_cfg("tests/cfg1.yaml").unwrap();
    assert!(matches!(
        try_init_cfg("tests/cfg1.yaml"),
        Err(ConfigError::AlreadyInitialized)
    ));
}
