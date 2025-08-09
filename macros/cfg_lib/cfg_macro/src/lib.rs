use proc_macro2::{Ident, TokenTree};
use quote::{format_ident, quote};
use syn::{DeriveInput};
use proc_macro::TokenStream;

/// ```example
/// #[conf(path="可选",prefix="可选",data_type="可选",lib="可选",check=[true,false]可选)]
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
/// lib 指定yml与cfg_lib库。默认使用common
///check 初始化struct时，默认None,不校验。some(true)开启自定义字段检查；需手动实现trait cfg_lib::CheckFromConf
#[proc_macro_attribute]
pub fn conf(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(item).expect("syn parse item failed");
    let attr = parse_attr(attrs);
    let struct_name = &ast.ident;
    let register_constructor = build_register_constructor(&attr, struct_name);
    let token_stream = build_conf_constructor(attr);
    let (i, t, w) = ast.generics.split_for_impl();
    let fun = quote! {
        #ast
        impl #i #struct_name #t #w {
            #token_stream
        }
        #register_constructor
    };
    // println!("{}", &fun.to_string());
    fun.into()
}

fn build_register_constructor(attr: &ConAttr, struct_name: &Ident) -> proc_macro2::TokenStream {
    let check = if attr.check {
        quote! {
                    #struct_name::conf()._field_check()
                }
    } else {
        quote! {
                   {
                       let _ = #struct_name::conf();
                       Ok(())
                   }
                }
    };
    let reg_ins = format_ident!("register_instance_{}", camel_to_snake(struct_name.to_string().as_str()));
    match &attr.lib {
        None => quote! {
            #[base::ctor::ctor]
            fn #reg_ins() {
               base::cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || { #check });
            }
    },
        Some(lib) if !lib.is_empty() => {
            let lib_path: syn::Path = syn::parse_str(lib).expect("解析库路径失败");
            quote! {
            #[#lib_path::ctor::ctor]
            fn #reg_ins() {
               #lib_path::cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || { #check });
            }
        }
        }
        _ => {
            match attr.path {
                None => {
                    quote! {
                        #[ctor::ctor]
                        fn #reg_ins() {
                            cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || #check);
                        }
                    }
                }
                Some(_) => {
                    quote! {
                        #[ctor::ctor]
                        fn #reg_ins(){
                            let res:Result<(), cfg_lib::conf::FieldCheckError> = #check;
                            res.expect("实例化配置文件失败");
                        }
                    }
                }
            }
        }
    }
}

fn build_conf_constructor(attr: ConAttr) -> proc_macro2::TokenStream {
    let fn_body_path;
    let fn_body_prefix;
    let fn_body_data_type;
    let fn_body_use_lib;

    match attr.lib {
        None => {
            fn_body_use_lib = quote! {
                use base::cfg_lib;
                use base::serde_yaml;
            };
        }
        Some(lib) => {
            if !lib.is_empty() {
                let lib_path: syn::Path = syn::parse_str(&lib).expect("解析库路径失败");
                fn_body_use_lib = quote! {
                use #lib_path::cfg_lib;
                use #lib_path::serde_yaml;
            };
            } else { fn_body_use_lib = quote! {} }
        }
    }

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
            #fn_body_use_lib
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

    let mut key = String::new();
    for arg in args {
        match arg {
            TokenTree::Ident(ident) => {
                key = ident.to_string();
                if key.eq("lib") {
                    attr.lib = Some("".to_string());
                } else if key.eq("check") {
                    attr.check = true;
                }
            }
            TokenTree::Punct(_) => {} // 逗号等符号忽略
            TokenTree::Literal(lit) => {
                let lit_str = lit.to_string();
                let value = lit_str.trim_matches('"').to_string();
                match key.as_str() {
                    "lib" => attr.lib = Some(value),
                    "path" => attr.path = Some(value),
                    "prefix" => attr.prefix = Some(value),
                    "data_type" => attr.data_type = Some(value.to_uppercase()),
                    other => panic!("invalid attr name: {}", other),
                }
            }
            _ => panic!("Unsupported token in attributes"),
        }
    }
    attr
}

#[derive(Default, Debug)]
struct ConAttr {
    path: Option<String>,
    prefix: Option<String>,
    data_type: Option<String>,
    lib: Option<String>,
    check: bool,
}

fn camel_to_snake(s: &str) -> String {
    let mut snake_case = String::new();

    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                snake_case.push('_');
            }
            snake_case.push(c.to_ascii_lowercase());
        } else {
            snake_case.push(c);
        }
    }

    snake_case
}