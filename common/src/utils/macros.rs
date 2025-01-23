#[macro_export]
macro_rules! serde_default {
    ($field:ident, $type:ty, $value:expr) => {
        fn $field() -> $type {
            $value
        }
    };
}

#[cfg(test)]
mod tests{
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug)]
    struct Config {
        #[serde(default = "default_host")]
        host: String,
        #[serde(default = "default_port")]
        port: u16,
    }
    serde_default!(default_host, String, "localhost".to_string());
    serde_default!(default_port, u16, 8080);
    #[test]
    fn test_default_value() {
        let config: Config = serde_json::from_str("{}").unwrap();
        println!("{:?}", config); // Config { host: "localhost", port: 8080 }
    }
}

