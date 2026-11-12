//! Compile-time catalog lookup keeps Rust's format checking and implicit captures.
use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, LitStr, Token};

struct Message {
    key: LitStr,
    arguments: Tokens,
}
impl Parse for Message {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key = input.parse()?;
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
        Ok(Self {
            key,
            arguments: input.parse()?,
        })
    }
}
fn catalogs() -> &'static (BTreeMap<String, String>, BTreeMap<String, String>) {
    static CATALOGS: OnceLock<(BTreeMap<String, String>, BTreeMap<String, String>)> =
        OnceLock::new();
    CATALOGS.get_or_init(|| {
        (
            serde_json::from_str(include_str!("../../../locales/rust/zh-CN.json"))
                .expect("valid Chinese catalog"),
            serde_json::from_str(include_str!("../../../locales/rust/en.json"))
                .expect("valid English catalog"),
        )
    })
}
fn message(key: &LitStr) -> syn::Result<(LitStr, LitStr)> {
    let (zh, en) = catalogs();
    let missing = || {
        syn::Error::new(
            key.span(),
            std::format!("missing locale key {}", key.value()),
        )
    };
    Ok((
        LitStr::new(zh.get(&key.value()).ok_or_else(missing)?, key.span()),
        LitStr::new(en.get(&key.value()).ok_or_else(missing)?, key.span()),
    ))
}
#[proc_macro]
pub fn text(input: TokenStream) -> TokenStream {
    let key = parse_macro_input!(input as LitStr);
    match message(&key) {
        Ok((zh, en)) => quote! {
            match ::summon_core::i18n::language() {
                ::summon_core::i18n::Language::Chinese => #zh,
                ::summon_core::i18n::Language::English => #en,
            }
        }
        .into(),
        Err(e) => e.into_compile_error().into(),
    }
}
#[proc_macro]
pub fn format(input: TokenStream) -> TokenStream {
    let Message { key, arguments } = parse_macro_input!(input as Message);
    match message(&key) {
        Ok((zh, en)) => quote! {
            match ::summon_core::i18n::language() {
                ::summon_core::i18n::Language::Chinese => ::std::format!(#zh, #arguments),
                ::summon_core::i18n::Language::English => ::std::format!(#en, #arguments),
            }
        }
        .into(),
        Err(e) => e.into_compile_error().into(),
    }
}
/// A fixed source token for recognizers, patterns, and embedded machine artifacts.
#[proc_macro]
pub fn source(input: TokenStream) -> TokenStream {
    let key = parse_macro_input!(input as LitStr);
    match message(&key) {
        Ok((zh, _)) => quote!(#zh).into(),
        Err(e) => e.into_compile_error().into(),
    }
}

/// Embed both versions of a shipped text asset, with compiler dependency tracking.
#[proc_macro]
pub fn include_text(input: TokenStream) -> TokenStream {
    let path = parse_macro_input!(input as LitStr);
    if path.value().split('/').any(|part| part == "..") || path.value().starts_with('/') {
        return syn::Error::new(
            path.span(),
            "asset paths must be relative to locales/assets",
        )
        .into_compile_error()
        .into();
    }
    quote! {
        match ::summon_core::i18n::language() {
            ::summon_core::i18n::Language::Chinese => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../locales/assets/zh-CN/", #path)),
            ::summon_core::i18n::Language::English => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../locales/assets/en/", #path)),
        }
    }.into()
}
