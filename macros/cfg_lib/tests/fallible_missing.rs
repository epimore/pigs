use cfg_lib::conf::{try_get_config, try_init_cfg, ConfigError};

#[test]
fn missing_config_and_uninitialized_read_return_typed_errors() {
    assert!(matches!(try_get_config(), Err(ConfigError::NotInitialized)));
    let error = try_init_cfg("tests/does-not-exist.yml").unwrap_err();
    assert!(matches!(error, ConfigError::Open { .. }));
}
