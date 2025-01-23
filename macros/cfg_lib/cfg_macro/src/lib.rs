
extern crate proc_macro;

use proc_macro2::TokenTree;
use quote::quote;
use syn::{DeriveInput};
use proc_macro::TokenStream;
/// ```example
/// #[conf(path="可选",prefix="可选",data_type="可选")]
/// struct T{ xxx}
/// impl struct{
///   fn test()->Self{
///     T::conf()
///   }
/// }
/// ```
/// attributes(path, prefix, data_type)
/// 配合serde 从指定文件中format数据到struct；
/// path:指定文件 默认读取配置文件;
/// prefix: 指定字段数据 默认无;
/// data_type: 文件类型 默认yaml,暂仅支持yaml;
#[proc_macro_attribute]
pub fn conf(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(item).expect("syn parse item failed");
    let attr = parse_attr(attrs);
    let token_stream = build_fn_constructor(attr);
    let name = &ast.ident;
    let (i, t, w) = ast.generics.split_for_impl();
    let fun = quote! {
        #ast
        impl #i #name #t #w {
            #token_stream
        }
    };
    // println!("{}",&fun.to_string());
    fun.into()
}

fn build_fn_constructor(attr: ConAttr) -> proc_macro2::TokenStream {
    let fn_body_path;
    let fn_body_prefix;
    let fn_body_data_type;

    match attr.path {
        None => {
            fn_body_path = quote! {
                let yaml_content = cfg_lib::conf::get_config();
                let yaml_value: serde_yaml::Value = serde_yaml::from_str(&yaml_content)
                    .expect("Failed to parse YAML content");
            };
        }
        Some(path) => {
            fn_body_path = quote! {
                let yaml_content = std::fs::read_to_string(#path)
                    .expect("Failed to read YAML file");
                let yaml_value: serde_yaml::Value = serde_yaml::from_str(&yaml_content)
                    .expect("Failed to parse YAML");
            };
        }
    }

    match attr.prefix {
        None => {
            fn_body_prefix = quote! {
                let target_value = &yaml_value;
            };
        }
        Some(prefix) => {
            fn_body_prefix = quote! {
                let mut target_value = &yaml_value;
                for key in #prefix.split('.') {
                    if let serde_yaml::Value::Mapping(map) = target_value {
                        target_value = map.get(&serde_yaml::Value::String(key.to_string()))
                            .expect("Specified prefix not found in YAML");
                    } else {
                        panic!("Invalid YAML structure for the specified prefix");
                    }
                }
            };
        }
    }

    match attr.data_type.as_deref() {
        None | Some("YAML") => {
            fn_body_data_type = quote! {
                serde_yaml::from_value(target_value.clone())
                    .expect("Failed to map YAML value to struct")
            };
        }
        Some(data_type) => {
            panic!("暂不支持格式: {}", data_type);
        }
    }

    quote! {
        fn conf() -> Self {
            #fn_body_path
            #fn_body_prefix
            #fn_body_data_type
        }
    }
}

fn parse_attr(attrs: TokenStream) -> ConAttr {
    let args = proc_macro2::TokenStream::from(attrs);
    let mut attr = ConAttr::default();
    if args.is_empty() {
        return attr;
    }
    let mut key = "".to_string();
    for arg in args {
        match arg {
            TokenTree::Group(_) => {}
            TokenTree::Ident(ident) => {
                key = ident.to_string();
            }
            TokenTree::Punct(_) => {}
            TokenTree::Literal(lit) => {
                let value = lit.to_string().trim_matches('"').to_string(); // 去掉引号
                match &*key {
                    "path" => {
                        attr.path = Some(value);
                    }
                    "prefix" => {
                        attr.prefix = Some(value);
                    }
                    "data_type" => {
                        attr.data_type = Some(value.to_uppercase());
                    }
                    other => {
                        panic!("invalid attr name: {}", other);
                    }
                }
            }
        }
    }
    attr
}

#[derive(Default)]
struct ConAttr {
    path: Option<String>,
    prefix: Option<String>,
    data_type: Option<String>,
}
