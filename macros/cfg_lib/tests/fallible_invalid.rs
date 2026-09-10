use cfg_lib::conf::{try_init_cfg, ConfigError};
use cfg_macro::conf;
use serde::Deserialize;

#[derive(Deserialize)]
#[conf(lib)]
struct InvalidConfig {
    #[allow(dead_code)]
    required_number: u64,
}

#[test]
fn invalid_typed_config_returns_parse_error() {
    let error = try_init_cfg("tests/invalid.yaml").unwrap_err();
    assert!(matches!(error, ConfigError::Parse { .. }));
}
