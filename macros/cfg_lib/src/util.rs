#[macro_export]
macro_rules! load_file_yaml_field_to_struct {
    ($file_path:expr, $field:expr, $struct_type:ty) => {{
        use std::fs::File;
        use std::io::Read;
        use serde_yaml::{Value, from_str};
        // 读取 YAML 文件
        let mut file = File::open($file_path).expect("File not found");
        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("Failed to read file");

        // 解析 YAML 文件到 serde_yaml::Value
        let yaml_value: Value = serde_yaml::from_str(&contents).expect("Failed to parse YAML");

        // 提取指定字段
        let field_value = &yaml_value[$field];

        // 将字段的内容映射到结构体
        let result: $struct_type = serde_yaml::from_value(field_value.clone())
            .expect(&format!("Failed to map field '{}' to struct", $field));

        result
    }};
}
#[macro_export]
macro_rules! load_str_yaml_field_to_struct {
    ($the_str:expr,$field:expr, $struct_type:ty) => {{
        use serde_yaml::{Value, from_str};

        let yaml_value: Value = serde_yaml::from_str($the_str).expect("Failed to parse YAML");
        let field_value = &yaml_value[$field];
        let result: $struct_type = serde_yaml::from_value(field_value.clone())
            .expect(&format!("Failed to map field '{}' to struct", $field));
        result
    }};
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Features {
        logging: bool,
        metrics: bool,
    }

    #[test]
    fn test_load_file_yaml_field_to_struct() {
        // 使用宏只映射 'features' 字段到 Features 结构体
        let features: Features = load_file_yaml_field_to_struct!("/home/ubuntu20/code/rs/mv/github/epimore/pig/macros/cfg/tests/cfg1.yaml", "features", Features);
        // 打印 features 结果
        println!("Parsed features: {:?}", features);
    }
    #[test]
    fn test_load_str_yaml_field_to_struct() {
        use std::fs::File;
        use std::io::Read;
        // 读取 YAML 文件
        let mut file = File::open("/home/ubuntu20/code/rs/mv/github/epimore/pig/macros/cfg/tests/cfg1.yaml").expect("File not found");
        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("Failed to read file");
        let features: Features = load_str_yaml_field_to_struct!(&contents, "features", Features);
        // 打印 features 结果
        println!("Parsed features: {:?}", features);
    }
}