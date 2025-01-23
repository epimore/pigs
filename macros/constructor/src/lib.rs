use proc_macro::TokenStream;
use proc_macro2::Ident;
use quote::{format_ident, quote};
use syn::{Attribute, Data, DeriveInput, Field, Fields, Index, Meta, Token, Type};
use syn::parse::Parse;
use syn::punctuated::Punctuated;
use syn::token::Comma;

const IDENT_GET: &str = "get";
const IDENT_SET: &str = "set";
const IDENT_NEW: &str = "new";

#[proc_macro_derive(Get, attributes(get))]
pub fn drive_get(input: TokenStream) -> TokenStream {
    drive(input, IDENT_GET)
}

#[proc_macro_derive(Set, attributes(set))]
pub fn drive_set(input: TokenStream) -> TokenStream {
    drive(input, IDENT_SET)
}

#[proc_macro_derive(New, attributes(new))]
pub fn drive_new(input: TokenStream) -> TokenStream {
    drive(input, IDENT_NEW)
}

fn drive(input: TokenStream, ident: &str) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("syn parse failed");
    let res = match ast.data {
        Data::Struct(ref s) => { handle_struct(&ast, &s.fields, ident) }
        Data::Enum(_) => { panic!("enum is not supported") }
        Data::Union(_) => { panic!("union is not supported") }
    };
    res.into()
}

//true:named
//false:unnamed
enum ConstructorIdent {
    Get(bool),
    Set(bool),
    New(bool),
}

fn handle_struct(ast: &DeriveInput, fields: &Fields, ident: &str) -> proc_macro2::TokenStream {
    match *fields {
        Fields::Named(ref fields) => {
            match ident {
                IDENT_GET => {
                    let constructor_ident = ConstructorIdent::Get(true);
                    build_constructor(ast, &fields.named, constructor_ident)
                }
                IDENT_SET => {
                    let constructor_ident = ConstructorIdent::Set(true);
                    build_constructor(ast, &fields.named, constructor_ident)
                }
                IDENT_NEW => {
                    let constructor_ident = ConstructorIdent::New(true);
                    build_constructor(ast, &fields.named, constructor_ident)
                }
                &_ => { panic!("invalid ident") }
            }
        }
        Fields::Unnamed(ref fields) => {
            match ident {
                IDENT_GET => {
                    let constructor_ident = ConstructorIdent::Get(false);
                    build_constructor(ast, &fields.unnamed, constructor_ident)
                }
                IDENT_SET => {
                    let constructor_ident = ConstructorIdent::Set(false);
                    build_constructor(ast, &fields.unnamed, constructor_ident)
                }
                IDENT_NEW => {
                    let constructor_ident = ConstructorIdent::New(false);
                    build_constructor(ast, &fields.unnamed, constructor_ident)
                }
                &_ => { panic!("invalid ident") }
            }
        }
        Fields::Unit => { panic!("Unit struct is not supported") }
    }
}

fn build_constructor(ast: &DeriveInput, fields: &Punctuated<Field, Token![,]>, constructor_ident: ConstructorIdent) -> proc_macro2::TokenStream {
    let constructors = match constructor_ident {
        ConstructorIdent::Get(true) => {
            build_et_named_fn(build_named_get_constructor, ast, fields, IDENT_GET)
        }
        ConstructorIdent::Get(false) => {
            build_et_unnamed_fn(build_unnamed_get_constructor, ast, fields, IDENT_GET)
        }
        ConstructorIdent::Set(true) => {
            build_et_named_fn(build_named_set_constructor, ast, fields, IDENT_SET)
        }
        ConstructorIdent::Set(false) => {
            build_et_unnamed_fn(build_unnamed_set_constructor, ast, fields, IDENT_SET)
        }
        ConstructorIdent::New(true) => {
            build_named_new_constructor(ast, fields, IDENT_NEW)
        }
        ConstructorIdent::New(false) => {
            build_unnamed_new_constructor(ast, fields, IDENT_NEW)
        }
    };
    let name = &ast.ident;
    let (i, t, w) = ast.generics.split_for_impl();
    quote! {
        #[automatically_derived]
        impl #i #name #t #w {
            #constructors
        }
    }
}


fn build_unnamed_new_constructor(ast: &DeriveInput, fields: &Punctuated<Field, Token![,]>, ident: &str) -> proc_macro2::TokenStream {
    let args = parse_args::<Index>(&ast.attrs, ident);
    let mut field_vars = Vec::new();
    let mut field_names = Vec::new();
    let mut field_types = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let index = Index::from(index);
        if contains_fields(&args, &index) {
            let field_type = &field.ty;
            let field_name = format_ident!("field_{}",index);
            field_names.push(field_name.clone());
            field_types.push(field_type);
            field_vars.push(quote!(#field_name));
        } else {
            field_vars.push(quote!(Default::default()));
        }
    }
    quote! {
            pub fn new(#(#field_names:#field_types),*)->Self{
                Self(#(#field_vars),*)
            }
        }
}

fn build_named_new_constructor(ast: &DeriveInput, fields: &Punctuated<Field, Token![,]>, ident: &str) -> proc_macro2::TokenStream {
    let args = parse_args::<Ident>(&ast.attrs, ident);
    let mut b = false;
    let (field_names, field_types): (Vec<Ident>, Vec<Type>) = fields.iter().map(|field| {
        let field_name = field.ident.as_ref().unwrap();
        (field_name, field)
    }).filter(|(field_name, _field)| {
        let t = contains_fields(&args, field_name);
        if !t {
            b = true;
        }
        t
    })
        .map(|(field_name, field)| {
            let field_type = &field.ty;
            (field_name.clone(), field_type.clone())
        }).unzip();
    if b {
        quote! {
            pub fn new(#(#field_names:#field_types),*)->Self{
                Self{#(#field_names),*,..Default::default()}
            }
        }
    } else {
        quote! {
            pub fn new(#(#field_names:#field_types),*)->Self{
                Self{#(#field_names),*}
            }
        }
    }
}

fn build_et_named_fn<F>(f: F, ast: &DeriveInput, fields: &Punctuated<Field, Token![,]>, ident: &str) -> proc_macro2::TokenStream
    where F: Fn(&Ident, &Type, Ident) -> proc_macro2::TokenStream {
    let args = parse_args::<Ident>(&ast.attrs, ident);
    let mut constructors = quote!();
    for field in fields {
        let field_type = &field.ty;
        let field_name = field.ident.as_ref().unwrap();
        if !contains_fields(&args, field_name) {
            continue;
        }
        let constructor_name = format_ident!("{ident}_{}",field_name);
        let constructor = f(field_name, field_type, constructor_name);
        constructors = quote! {
            #constructors
            #constructor
        };
    }
    constructors
}

fn build_et_unnamed_fn<F>(f: F, ast: &DeriveInput, fields: &Punctuated<Field, Token![,]>, ident: &str) -> proc_macro2::TokenStream
    where F: Fn(&Index, &Type, Ident) -> proc_macro2::TokenStream {
    let args = parse_args::<Index>(&ast.attrs, ident);
    let mut constructors = quote!();
    for (index, field) in fields.iter().enumerate() {
        let field_type = &field.ty;
        let index = Index::from(index);
        if !contains_fields(&args, &index) {
            continue;
        }
        let constructor_name = format_ident!("{ident}_{}",index);
        let constructor = f(&index, field_type, constructor_name);
        constructors = quote! {
            #constructors
            #constructor
        };
    }
    constructors
}

fn build_unnamed_get_constructor(index: &Index, field_type: &Type, constructor_name: Ident) -> proc_macro2::TokenStream {
    let constructor = quote! {
            pub fn #constructor_name(&self) ->&#field_type{
                &self.#index
            }
        };
    constructor
}

fn build_named_get_constructor(field_name: &Ident, field_type: &Type, constructor_name: Ident) -> proc_macro2::TokenStream {
    let constructor = quote! {
            pub fn #constructor_name(&self) -> &#field_type{
                &self.#field_name
            }
        };
    constructor
}

fn build_unnamed_set_constructor(index: &Index, field_type: &Type, constructor_name: Ident) -> proc_macro2::TokenStream {
    let param_name = format_ident!("field_{}",index);
    let constructor = quote! {
            pub fn #constructor_name(&mut self,#param_name:impl Into<#field_type>) {
                self.#index = #param_name.into();
            }
        };
    constructor
}

fn build_named_set_constructor(field_name: &Ident, field_type: &Type, constructor_name: Ident) -> proc_macro2::TokenStream {
    let constructor = quote! {
            pub fn #constructor_name(&mut self,#field_name:impl Into<#field_type>) {
                self.#field_name = #field_name.into();
            }
        };
    constructor
}

fn parse_args<T: Parse>(attrs: &Vec<Attribute>, ident: &str) -> Option<Punctuated<T, Comma>> {
    if let Some(attr) = attrs.iter().find(|attr| attr.path().is_ident(ident)) {
        match &attr.meta {
            Meta::Path(_) => { panic!("`{ident}` attribute should like `#[{ident}(a, b, c)]`") }
            Meta::List(list) => {
                Some(list.parse_args_with(Punctuated::<T, Comma>::parse_terminated).expect("parse args failed"))
            }
            Meta::NameValue(_) => { panic!("`{ident}` attribute should like `#[{ident}(a, b, c)]`") }
        }
    } else {
        None
    }
}

//未指定字段：全部字段为真
//指定字段时：指定字段为真
fn contains_fields<T: Parse + PartialEq>(args: &Option<Punctuated<T, Comma>>, item: &T) -> bool {
    args.as_ref()
        .map(|arg_list| arg_list.iter().any(|arg| arg == item))
        .unwrap_or(true)
}