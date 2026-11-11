//! Derives for Skyhook's enums and configuration types, which `skyhook_base` exports
//! beside the traits they implement.

mod decode;

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{
    Attribute, Data, DeriveInput, Fields, Ident, LitStr, Meta, Token, punctuated::Punctuated,
};

/// A unit enum with one spelling per variant, which JSON, the session database's
/// dictionaries, `as_str`, `FromStr` and `Display` all share: serde's, from `rename`, the
/// enum's `rename_all = "snake_case"`, or the variant's name. An `alias` parses and
/// deserializes to its variant but is never emitted. It generates `ALL`, `as_str`, the
/// `NamedEnum` trait, `FromStr`, `Deserialize`, which lists the spellings when it refuses a
/// value, and `Display`, unless variants carry `#[error]` messages to display instead.
/// It decodes from configuration as it deserializes. `Serialize` and `JsonSchema` derive as
/// usual from the same attributes.
///
/// ```ignore
/// #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, NamedEnum)]
/// #[serde(rename_all = "snake_case")]
/// enum Flavour {
///     #[serde(alias = "original")]
///     Plain,
///     #[serde(rename = "salt+vinegar")]
///     SaltAndVinegar,
/// }
/// ```
#[proc_macro_derive(NamedEnum, attributes(serde))]
pub fn named_enum(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    named(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A configuration type decoded from a mapping: its keys, each read as its field's type,
/// the rest refused with every key it takes, and every key it requires and lacks refused
/// together. serde's `rename`, `default`, `flatten` and `skip` spell a field as they do for
/// `Serialize`, and the container's `default` takes a left-out field from `Default`.
/// `#[decode(source)]` gives a `Source` field where the mapping is written,
/// `#[decode(with = "path")]` reads a field by `fn(&Node, &mut Context) -> Option<T>`,
/// `#[decode(check = "path")]` runs `fn(&mut Self, &Node, &mut Context)` on the value, and
/// `#[decode(bound = "...")]` bounds the impls. An enum is read by its `tag`, as serde or
/// `#[decode(tag = "key")]` names it, whose value names the variant: one flattened value or
/// its own fields. A `transparent` struct decodes as its one field.
#[proc_macro_derive(Decode, attributes(decode, serde))]
pub fn decode(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    decode::derive(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A data enum's kinds: a unit enum, as visible as the data enum, with a variant of the same
/// name for each of its variants, and `kind()`, which names a value's. The kind enum derives
/// `Clone, Copy, Debug, PartialEq, Eq, Serialize` and `NamedEnum`, spelled by the data enum's
/// `serde(rename_all)` rule, whether or not the data enum derives serde's traits; its other
/// serde attributes are not the kind's. `#[kinds(name = Kind)]` names it and its other items are further
/// attributes, such as more derives. A variant's `cfg` applies to its kind too.
///
/// ```ignore
/// #[derive(Kinds)]
/// #[kinds(name = LastKind, derive(Hash))]
/// #[serde(rename_all = "snake_case")]
/// enum Last {
///     Awaiting,
///     Failed(Failure),
/// }
/// ```
#[proc_macro_derive(Kinds, attributes(kinds, serde))]
pub fn kinds(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    kinded(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn kinded(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "Kinds names an enum's variants",
        ));
    };
    let (mut kind, mut attrs) = (None, Vec::new());
    for attr in input
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("kinds"))
    {
        for meta in attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)? {
            match meta {
                Meta::NameValue(name) if name.path.is_ident("name") => {
                    kind = Some(syn::parse2::<Ident>(name.value.into_token_stream())?);
                }
                meta => attrs.push(meta),
            }
        }
    }
    let kind = kind.ok_or_else(|| {
        syn::Error::new_spanned(&input.ident, "Kinds needs `#[kinds(name = ...)]`")
    })?;
    let mut rename_all = None;
    for attr in serde(&input.attrs) {
        attr.parse_nested_meta(|meta| {
            if meta.input.peek(Token![=]) {
                let value: syn::Expr = meta.value()?.parse()?;
                if meta.path.is_ident("rename_all") {
                    rename_all = Some(value);
                }
            } else if meta.input.peek(syn::token::Paren) {
                meta.input.parse::<proc_macro2::TokenTree>()?;
            }
            Ok(())
        })?;
    }
    let rename_all = rename_all.map(|rule| quote!(#[serde(rename_all = #rule)]));
    let vis = &input.vis;
    let mut variants = Vec::new();
    let mut arms = Vec::new();
    for variant in &data.variants {
        let ident = &variant.ident;
        let cfgs: Vec<_> = (variant.attrs.iter())
            .filter(|attr| attr.path().is_ident("cfg"))
            .collect();
        variants.push(quote! { #(#cfgs)* #ident });
        arms.push(quote! { #(#cfgs)* Self::#ident { .. } => #kind::#ident });
    }
    let name = &input.ident;
    let (generics, types, bounds) = input.generics.split_for_impl();
    Ok(quote! {
        #[derive(
            ::std::clone::Clone, ::std::marker::Copy, ::std::fmt::Debug, ::std::cmp::PartialEq,
            ::std::cmp::Eq, ::serde::Serialize, ::skyhook_base::enums::NamedEnum,
        )]
        #(#[#attrs])*
        #rename_all
        #vis enum #kind { #(#variants),* }

        impl #generics #name #types #bounds {
            #vis const fn kind(&self) -> #kind {
                match self { #(#arms),* }
            }
        }
    })
}

/// The spellings serde gives a unit enum's variants: `rename`, else the enum's
/// `rename_all` rule, else the variant's name, with any `alias` parsed too.
struct Spelling {
    name: String,
    aliases: Vec<LitStr>,
}

fn named(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "NamedEnum spells an enum's variants",
        ));
    };
    let mut snake_case = false;
    for attr in serde(&input.attrs) {
        attr.parse_nested_meta(|meta| {
            let rule: LitStr = meta.value()?.parse()?;
            if !meta.path.is_ident("rename_all") || rule.value() != "snake_case" {
                return Err(
                    meta.error("NamedEnum spells only a `rename_all = \"snake_case\"` rule")
                );
            }
            snake_case = true;
            Ok(())
        })?;
    }
    let mut variants = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) || variant.discriminant.is_some() {
            return Err(syn::Error::new_spanned(
                variant,
                "NamedEnum spells only unit variants",
            ));
        }
        let ident = variant.ident.to_string();
        let mut spelling = Spelling {
            name: if snake_case { snake(&ident) } else { ident },
            aliases: Vec::new(),
        };
        for attr in serde(&variant.attrs) {
            attr.parse_nested_meta(|meta| {
                let text: LitStr = meta.value()?.parse()?;
                if meta.path.is_ident("rename") {
                    spelling.name = text.value();
                } else if meta.path.is_ident("alias") {
                    spelling.aliases.push(text);
                } else {
                    return Err(meta.error("NamedEnum spells only `rename` and `alias`"));
                }
                Ok(())
            })?;
        }
        variants.push((&variant.ident, spelling));
    }
    let name = &input.ident;
    let count = variants.len();
    let idents: Vec<_> = variants.iter().map(|(ident, _)| *ident).collect();
    let names: Vec<_> = variants
        .iter()
        .map(|(_, spelling)| &spelling.name)
        .collect();
    let parsed = variants.iter().map(|(ident, spelling)| {
        let (name, aliases) = (&spelling.name, &spelling.aliases);
        quote! { #name #(| #aliases)* => ::std::option::Option::Some(Self::#ident) }
    });
    let base = quote!(::skyhook_base::enums);
    // An enum whose variants carry error messages displays those instead.
    let displays_messages = (input.attrs.iter())
        .chain(data.variants.iter().flat_map(|variant| &variant.attrs))
        .any(|attr| attr.path().is_ident("error"));
    let display = (!displays_messages).then(|| {
        quote! {
            impl ::std::fmt::Display for #name {
                fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    formatter.write_str(self.as_str())
                }
            }
        }
    });
    Ok(quote! {
        impl #name {
            pub const ALL: [Self; #count] = [#(Self::#idents),*];

            pub const fn as_str(self) -> &'static str {
                match self { #(Self::#idents => #names),* }
            }
        }

        impl #base::NamedEnum for #name {
            const ALL: &'static [Self] = &Self::ALL;
            const NAMES: &'static [&'static str] = &[#(#names),*];

            fn as_str(self) -> &'static str {
                Self::as_str(self)
            }

            fn parse(value: &str) -> ::std::option::Option<Self> {
                match value {
                    #(#parsed,)*
                    _ => ::std::option::Option::None,
                }
            }
        }

        impl ::std::str::FromStr for #name {
            type Err = #base::UnknownName;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                <Self as #base::NamedEnum>::parse(value)
                    .ok_or_else(|| #base::UnknownName::of::<Self>(stringify!(#name), value))
            }
        }

        impl ::skyhook_base::decode::Decode for #name {
            fn decode(
                node: &::skyhook_base::decode::Node,
                cx: &mut ::skyhook_base::decode::Context,
            ) -> ::std::option::Option<Self> {
                #base::decode(node, cx)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for #name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                #base::deserialize(deserializer)
            }
        }

        #display
    })
}

fn serde(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("serde"))
}

/// `name` as serde's `snake_case` rule spells a variant.
pub(crate) fn snake(name: &str) -> String {
    let mut snake = String::new();
    for (index, ch) in name.char_indices() {
        if index > 0 && ch.is_uppercase() {
            snake.push('_');
        }
        snake.push(ch.to_ascii_lowercase());
    }
    snake
}
