use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenTree};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields};

/// ```example
/// #[conf(path = "optional", prefix = "optional", data_type = "optional", lib = "optional", check, default)]
/// struct T { ... }
/// ```
///
/// Attributes:
/// - `path`: target file path.
/// - `prefix`: nested yaml key path split by `.`.
/// - `data_type`: defaults to yaml.
/// - `lib`: use external crate path, empty means current crate.
/// - `check`: run `CheckFromConf::_field_check`.
/// - `default`: enabled by default; It is only disabled when default = "false".
///    Use #[serde(default = "...")] or #[serde(default)] to set default value for a field.
#[proc_macro_attribute]
pub fn conf(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(item).expect("syn parse item failed");
    let attr = parse_attr(attrs);
    let struct_name = &ast.ident;
    let register_constructor = build_register_constructor(&attr, struct_name);
    let token_stream = build_conf_constructor(attr, &ast);
    let (i, t, w) = ast.generics.split_for_impl();
    let fun = quote! {
        #ast
        impl #i #struct_name #t #w {
            #token_stream
        }
        #register_constructor
    };
    fun.into()
}

fn build_register_constructor(attr: &ConAttr, struct_name: &Ident) -> proc_macro2::TokenStream {
    if attr.path.is_some() {
        return quote! {};
    }

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
    let reg_ins = format_ident!(
        "register_instance_{}",
        camel_to_snake(struct_name.to_string().as_str())
    );
    match &attr.lib {
        None => quote! {
            #[base::ctor::ctor]
            fn #reg_ins() {
                base::cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || { #check });
            }
        },
        Some(lib) if !lib.is_empty() => {
            let lib_path: syn::Path = syn::parse_str(lib).expect("parse lib path failed");
            quote! {
                #[#lib_path::ctor::ctor]
                fn #reg_ins() {
                    #lib_path::cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || { #check });
                }
            }
        }
        _ => {
            quote! {
                #[ctor::ctor]
                fn #reg_ins() {
                    cfg_lib::conf::register_function(std::any::type_name::<#struct_name>(), || #check);
                }
            }
        }
    }
}

fn build_conf_constructor(attr: ConAttr, ast: &DeriveInput) -> proc_macro2::TokenStream {
    let fn_body_path;
    let fn_body_prefix;
    let fn_body_data_type;
    let fn_body_use_lib;

    match attr.lib {
        None => {
            fn_body_use_lib = quote! {
                use base::cfg_lib;
                use base::serde;
                use base::serde_yaml;
            };
        }
        Some(lib) => {
            if !lib.is_empty() {
                let lib_path: syn::Path = syn::parse_str(&lib).expect("parse lib path failed");
                fn_body_use_lib = quote! {
                    use #lib_path::cfg_lib;
                    use #lib_path::serde;
                    use #lib_path::serde_yaml;
                };
            } else {
                fn_body_use_lib = quote! {
                    use serde;
                };
            }
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
                        target_value = map
                            .get(&serde_yaml::Value::String(key.to_string()))
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
            if attr.default {
                fn_body_data_type = build_default_data_type(ast);
            } else {
                fn_body_data_type = quote! {
                    serde_yaml::from_value(target_value.clone())
                        .expect("Failed to map YAML value to struct")
                };
            }
        }
        Some(data_type) => {
            panic!("unsupported data type: {}", data_type);
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

fn build_default_data_type(ast: &DeriveInput) -> proc_macro2::TokenStream {
    let fields = match &ast.data {
        Data::Struct(struct_data) => match &struct_data.fields {
            Fields::Named(named) => named.named.iter().collect::<Vec<_>>(),
            _ => panic!("#[conf(default)] only supports named-field structs"),
        },
        _ => panic!("#[conf(default)] only supports structs"),
    };

    let proxy_name = format_ident!("__CfgMacroDefaultProxyFor{}", ast.ident);
    let struct_serde_attrs = ast.attrs.iter().filter(|attr| attr.path().is_ident("serde"));

    let proxy_fields = fields.iter().map(|field| {
        let field_name = field.ident.as_ref().expect("named field");
        let field_ty = &field.ty;
        let serde_attrs = field
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("serde"));
        quote! {
            #(#serde_attrs)*
            #field_name: #field_ty
        }
    });

    let assign_fields = fields.iter().map(|field| {
        let field_name = field.ident.as_ref().expect("named field");
        quote! {
            #field_name: __cfg_proxy.#field_name
        }
    });

    quote! {
        #[derive(serde::Deserialize)]
        #(#struct_serde_attrs)*
        struct #proxy_name {
            #(#proxy_fields,)*
        }

        let __cfg_proxy: #proxy_name = serde_yaml::from_value(target_value.clone())
            .expect("Failed to map YAML value to struct with defaults");

        Self {
            #(#assign_fields,)*
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
                let ident_str = ident.to_string();
                if key.eq("default") && (ident_str.eq("true") || ident_str.eq("false")) {
                    attr.default = !ident_str.eq("false");
                    key.clear();
                    continue;
                }
                key = ident_str;
                if key.eq("lib") {
                    attr.lib = Some(String::new());
                } else if key.eq("check") {
                    attr.check = true;
                } else if key.eq("default") {
                    attr.default = true;
                }
            }
            TokenTree::Punct(_) => {}
            TokenTree::Literal(lit) => {
                let lit_str = lit.to_string();
                let value = lit_str.trim_matches('"').to_string();
                match key.as_str() {
                    "lib" => attr.lib = Some(value),
                    "path" => attr.path = Some(value),
                    "prefix" => attr.prefix = Some(value),
                    "data_type" => attr.data_type = Some(value.to_uppercase()),
                    "default" => {
                        attr.default = match value.to_ascii_lowercase().as_str() {
                            "false" => false,
                            "true" => true,
                            _ => panic!("invalid default attr value: {}", value),
                        }
                    }
                    other => panic!("invalid attr name: {}", other),
                }
            }
            _ => panic!("Unsupported token in attributes"),
        }
    }
    attr
}

#[derive(Debug)]
struct ConAttr {
    path: Option<String>,
    prefix: Option<String>,
    data_type: Option<String>,
    lib: Option<String>,
    check: bool,
    default: bool,
}

impl Default for ConAttr {
    fn default() -> Self {
        Self {
            path: None,
            prefix: None,
            data_type: None,
            lib: None,
            check: false,
            default: true,
        }
    }
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
