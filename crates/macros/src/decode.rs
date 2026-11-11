//! `#[derive(Decode)]`: a configuration type read from a mapping's keys.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Attribute, Data, DeriveInput, Fields, Ident, LitStr, Token, Type};

/// What a field's attributes, serde's or the decoder's own, say about reading it.
#[derive(Default)]
struct FieldSpec {
    rename: Option<String>,
    default: Option<Option<syn::Path>>,
    flatten: bool,
    skip: bool,
    source: bool,
    with: Option<syn::Path>,
}

#[derive(Default)]
struct ContainerSpec {
    default: bool,
    check: Option<syn::Path>,
    bound: Option<syn::WhereClause>,
    tag: Option<String>,
    snake_case: bool,
    transparent: bool,
}

fn skip_value(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    if meta.input.peek(Token![=]) {
        meta.value()?.parse::<syn::Expr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.input.parse::<proc_macro2::TokenTree>()?;
    }
    Ok(())
}

fn attributes(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    let read = |attr: &&Attribute| attr.path().is_ident("serde") || attr.path().is_ident("decode");
    attrs.iter().filter(read)
}

fn container(attrs: &[Attribute]) -> syn::Result<ContainerSpec> {
    let mut spec = ContainerSpec::default();
    for attr in attributes(attrs) {
        let own = attr.path().is_ident("decode");
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") && !meta.input.peek(Token![=]) {
                spec.default = true;
            } else if meta.path.is_ident("rename_all") {
                let rule = meta.value()?.parse::<LitStr>()?;
                if rule.value() != "snake_case" {
                    return Err(syn::Error::new_spanned(
                        rule,
                        "Decode spells only snake_case",
                    ));
                }
                spec.snake_case = true;
            } else if meta.path.is_ident("transparent") {
                spec.transparent = true;
            } else if meta.path.is_ident("tag") {
                spec.tag = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if own && meta.path.is_ident("check") {
                spec.check = Some(meta.value()?.parse::<LitStr>()?.parse()?);
            } else if own && meta.path.is_ident("bound") {
                let bound = meta.value()?.parse::<LitStr>()?.value();
                spec.bound = Some(syn::parse_str(&format!("where {bound}"))?);
            } else if own {
                return Err(meta.error("unknown decode attribute"));
            } else {
                skip_value(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(spec)
}

fn field_spec(attrs: &[Attribute]) -> syn::Result<FieldSpec> {
    let mut spec = FieldSpec::default();
    for attr in attributes(attrs) {
        let own = attr.path().is_ident("decode");
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                spec.rename = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("default") {
                spec.default = Some(match meta.input.peek(Token![=]) {
                    true => Some(meta.value()?.parse::<LitStr>()?.parse()?),
                    false => None,
                });
            } else if meta.path.is_ident("flatten") {
                spec.flatten = true;
            } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                spec.skip = true;
            } else if own && meta.path.is_ident("source") {
                spec.source = true;
            } else if own && meta.path.is_ident("with") {
                spec.with = Some(meta.value()?.parse::<LitStr>()?.parse()?);
            } else if own {
                return Err(meta.error("unknown decode attribute"));
            } else {
                skip_value(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(spec)
}

/// The keys and the decoding of a set of named fields, read from `map`.
struct Read {
    keys: TokenStream,
    lets: TokenStream,
    build: TokenStream,
}

fn named(fields: &syn::FieldsNamed, defaults: bool) -> syn::Result<Read> {
    let base = quote!(::skyhook_base::decode);
    let (mut keys, mut lets, mut build) = (Vec::new(), Vec::new(), Vec::new());
    for (index, field) in fields.named.iter().enumerate() {
        let ident = field.ident.as_ref().expect("named field");
        let ty: &Type = &field.ty;
        let spec = field_spec(&field.attrs)?;
        let var = format_ident!("__field{index}");
        let key = spec
            .rename
            .clone()
            .unwrap_or_else(|| ident.to_string().trim_start_matches("r#").to_owned());
        let read = if spec.source {
            quote!(::std::option::Option::Some(node.at.clone()))
        } else if spec.skip {
            quote!(::std::option::Option::Some(
                ::std::default::Default::default()
            ))
        } else if spec.flatten {
            keys.push(quote!(<#ty as #base::Fields>::keys(map, keys);));
            quote!(<#ty as #base::Fields>::decode_fields(node, map, cx, missing))
        } else {
            keys.push(quote!(keys.push(#key);));
            let field = match &spec.with {
                Some(with) => quote!(#base::field_with::<#ty>(map, #key, cx, missing, #with)),
                None => quote!(#base::field::<#ty>(map, #key, cx, missing)),
            };
            let absent = match (&spec.default, defaults) {
                (Some(Some(path)), _) => Some(quote!(#path())),
                (Some(None), _) => Some(quote!(::std::default::Default::default())),
                (None, true) => Some(quote!(__defaults.#ident)),
                (None, false) => None,
            };
            match absent {
                Some(absent) => quote! {
                    match map.contains(#key) {
                        true => #field.or_else(|| ::std::option::Option::Some(#absent)),
                        false => ::std::option::Option::Some(#absent),
                    }
                },
                None => field,
            }
        };
        lets.push(quote!(let #var: ::std::option::Option<#ty> = #read;));
        build.push(quote!(#ident: #var?));
    }
    Ok(Read {
        keys: quote!(#(#keys)*),
        lets: quote!(#(#lets)*),
        build: quote!(#(#build),*),
    })
}

pub(crate) fn derive(input: &DeriveInput) -> syn::Result<TokenStream> {
    let base = quote!(::skyhook_base::decode);
    let spec = container(&input.attrs)?;
    let name = &input.ident;
    let (generics, types, _) = input.generics.split_for_impl();
    let bound = &spec.bound;
    if spec.transparent {
        let fields = match &input.data {
            Data::Struct(data) => Vec::from_iter(&data.fields),
            _ => Vec::new(),
        };
        let [field] = fields[..] else {
            return Err(syn::Error::new_spanned(
                name,
                "transparent Decode reads one field",
            ));
        };
        let ty = &field.ty;
        let build = match &field.ident {
            Some(ident) => quote!(Self { #ident: value }),
            None => quote!(Self(value)),
        };
        return Ok(quote! {
            impl #generics #base::Decode for #name #types #bound {
                fn decode(
                    node: &#base::Node,
                    cx: &mut #base::Context,
                ) -> ::std::option::Option<Self> {
                    <#ty as #base::Decode>::decode(node, cx).map(|value| #build)
                }
            }
        });
    }
    let (keys, decode) = match &input.data {
        Data::Struct(data) => {
            let Fields::Named(fields) = &data.fields else {
                return Err(syn::Error::new_spanned(name, "Decode reads named fields"));
            };
            let Read { keys, lets, build } = named(fields, spec.default)?;
            let defaults = spec
                .default
                .then(|| quote!(let __defaults = <Self as ::std::default::Default>::default();));
            (
                keys,
                quote! {
                    #defaults
                    #lets
                    ::std::option::Option::Some(Self { #build })
                },
            )
        }
        Data::Enum(data) => {
            let Some(tag) = &spec.tag else {
                return Err(syn::Error::new_spanned(name, "Decode reads a tagged enum"));
            };
            let (mut names, mut variant_keys, mut arms) = (Vec::new(), Vec::new(), Vec::new());
            for variant in &data.variants {
                let ident: &Ident = &variant.ident;
                let rename = field_spec(&variant.attrs)?.rename;
                let spelled = rename.unwrap_or_else(|| match spec.snake_case {
                    true => crate::snake(&ident.to_string()),
                    false => ident.to_string(),
                });
                let (keys, arm) = match &variant.fields {
                    Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                        let ty = &fields.unnamed[0].ty;
                        (
                            quote!(<#ty as #base::Fields>::keys(map, keys);),
                            quote! {
                                <#ty as #base::Fields>::decode_fields(node, map, cx, missing)
                                    .map(Self::#ident)
                            },
                        )
                    }
                    Fields::Named(fields) => {
                        let Read { keys, lets, build } = named(fields, false)?;
                        (
                            keys,
                            quote!({ #lets ::std::option::Option::Some(Self::#ident { #build }) }),
                        )
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            variant,
                            "Decode reads a tagged variant's fields or one flattened value",
                        ));
                    }
                };
                arms.push(quote!(#spelled => #arm,));
                variant_keys.push((spelled.clone(), keys));
                names.push(spelled);
            }
            let chosen = variant_keys
                .iter()
                .map(|(spelled, keys)| quote!(::std::option::Option::Some(#spelled) => { #keys }));
            let every = variant_keys.iter().map(|(_, keys)| keys);
            (
                quote! {
                    keys.push(#tag);
                    match map.get(#tag).and_then(#base::Node::as_str) {
                        #(#chosen)*
                        _ => { #(#every)* }
                    }
                },
                quote! {
                    let ::std::option::Option::Some(tag) = map.get(#tag) else {
                        missing.push(#tag);
                        return ::std::option::Option::None;
                    };
                    let segment = || #base::Segment::Key(#tag.to_owned());
                    let ::std::option::Option::Some(variant) = tag.as_str() else {
                        cx.within(segment(), |cx| cx.expected(tag, "a string"));
                        return ::std::option::Option::None;
                    };
                    match variant {
                        #(#arms)*
                        other => {
                            let names = #base::one_of([#(#names),*]);
                            cx.within(segment(), |cx| cx.error(tag, ::std::format_args!(
                                "unknown variant `{other}`, expected one of {names}"
                            )));
                            ::std::option::Option::None
                        }
                    }
                },
            )
        }
        Data::Union(_) => return Err(syn::Error::new_spanned(name, "Decode reads no union")),
    };
    let checked = match &spec.check {
        Some(check) => quote! {
            let mut value = value?;
            if cx.refusals() == before {
                #check(&mut value, node, cx);
            }
            ::std::option::Option::Some(value)
        },
        None => quote!(value),
    };
    Ok(quote! {
        impl #generics #base::Fields for #name #types #bound {
            fn keys(map: &#base::Map, keys: &mut ::std::vec::Vec<&'static str>) {
                #keys
            }

            #[allow(unused_variables)]
            fn decode_fields(
                node: &#base::Node,
                map: &#base::Map,
                cx: &mut #base::Context,
                missing: &mut ::std::vec::Vec<&'static str>,
            ) -> ::std::option::Option<Self> {
                let before = cx.refusals();
                #[allow(unused_mut)]
                let mut read = || -> ::std::option::Option<Self> { #decode };
                let value = read();
                #checked
            }
        }

        impl #generics #base::Decode for #name #types #bound {
            fn decode(
                node: &#base::Node,
                cx: &mut #base::Context,
            ) -> ::std::option::Option<Self> {
                #base::decode_mapping::<Self>(node, cx)
            }
        }
    })
}
