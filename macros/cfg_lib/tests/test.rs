#[allow(dead_code, unused_imports)]
mod test1 {
    use serde::Deserialize;
    use serde::Deserializer;
    use cfg_lib::conf::{CheckFromConf, FieldCheckError, init_cfg};
    use cfg_macro::conf;


    #[derive(Debug, Deserialize)]
    #[conf(lib)]
    struct Cfg1 {
        name: String,
        version: String,
        features: Features,
    }

    #[test]
    fn test_default_conf1() {
        init_cfg("tests/cfg1.yaml".to_string());
        let conf = Cfg1::conf();
        println!("{:?}", conf);
    }

    #[derive(Debug, Deserialize)]
    #[conf(path = "tests/cfg1.yaml", lib)]
    struct Cfg2 {
        name: String,
        version: String,
        features: Features,
    }

    #[test]
    fn test_target_conf2() {
        let conf = Cfg2::conf();
        println!("{:?}", conf);
    }

    #[derive(Debug, Deserialize, Default)]
    #[conf(path = "tests/cfg1.yaml", prefix = "features", lib, check)]
    struct Features {
        logging: bool,
        metrics: bool,
        #[serde(default)]
        miss:u8,
    }

    #[derive(Debug, Deserialize)]
    #[conf(path = "tests/cfg1.yaml", prefix = "features", lib, default = "false")]
    struct FeaturesNoDefault {
        logging: bool,
        metrics: bool,
        miss: u8,
    }

    #[derive(Debug, Deserialize)]
    #[conf(path = "tests/cfg1.yaml", prefix = "features", lib)]
    struct FeaturesMissingWithoutSerdeDefault {
        logging: bool,
        metrics: bool,
        miss: u8,
    }

    fn miss_from_serde_default() -> u8 { 7 }

    #[derive(Debug, Deserialize)]
    #[conf(path = "tests/cfg1.yaml", prefix = "features", lib)]
    struct FeaturesSerdeDefault {
        logging: bool,
        metrics: bool,
        #[serde(default = "miss_from_serde_default")]
        miss: u8,
    }

    fn default_level() -> String {
        "info".to_string()
    }

    fn parse_level<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
    }

    #[derive(Debug, Deserialize)]
    #[conf(path = "tests/cfg1.yaml", prefix = "features", lib)]
    struct FeaturesSerdeDefaultWithDeserializer {
        logging: bool,
        metrics: bool,
        #[serde(deserialize_with = "parse_level")]
        #[serde(default = "default_level")]
        level: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    #[conf(path = "tests/cfg_struct_attr.yaml", prefix = "features", lib)]
    struct FeaturesContainerSerdeAttr {
        logging: bool,
        metrics: bool,
        miss_value: u8,
    }

    impl CheckFromConf for Features {
        fn _field_check(&self) -> Result<(), FieldCheckError> {
            if self.logging && self.metrics {
                let err_msg = "logging and metrics can't be true at the same time".to_string();
                println!("{}", &err_msg);
                // return Err(FieldCheckError::BizError(err_msg));
            }
            Ok(())
        }
    }

    #[test]
    fn test_prefix_conf() {
        let conf = Features::conf();
        assert_eq!(conf.miss, 0);
        println!("{:?}", conf);
    }

    #[test]
    #[should_panic(expected = "Failed to map YAML value to struct")]
    fn test_prefix_conf_default_false_should_panic() {
        let _ = FeaturesNoDefault::conf();
    }

    #[test]
    #[should_panic(expected = "Failed to map YAML value to struct with defaults")]
    fn test_prefix_conf_missing_without_serde_default_should_panic() {
        let _ = FeaturesMissingWithoutSerdeDefault::conf();
    }

    #[test]
    fn test_prefix_conf_serde_default_priority() {
        let conf = FeaturesSerdeDefault::conf();
        println!("{:?}",conf);
        assert_eq!(conf.miss, 7);
    }

    #[test]
    fn test_prefix_conf_serde_default_with_deserializer() {
        let conf = FeaturesSerdeDefaultWithDeserializer::conf();
        println!("{:?}", conf);
        assert_eq!(conf.level, "info");
    }

    #[test]
    fn test_prefix_conf_container_serde_attr() {
        let conf = FeaturesContainerSerdeAttr::conf();
        println!("{:?}", conf);
        assert_eq!(conf.miss_value, 9);
    }
}
