//! `#[derive(CredentialSchema)]` — turns a backend's credential enum into the
//! login schema the UI renders and the typed value `create_messenger` consumes.
//!
//! Each enum variant is one credential *shape*. A variant tagged
//! `#[credential(method = "Label")]` is a rendered login method (the UI builds a
//! form from its fields); a variant tagged `#[credential(hidden)]` is a valid,
//! parseable shape (e.g. a saved session reconstructed from disk) that the UI
//! does not render. Both appear in `auth_methods`, so it is the complete set of
//! field keys a backend understands.
//!
//! A variant's named fields become the credential fields. The field *identifier*
//! is the stable storage/transport key; its `#[credential(label = "...")]` and
//! optional `secret` flag drive the prompt; an `Option<_>` field is optional.
//! The value type is opaque to the schema — it just round-trips a `SecureString`
//! — so this macro carries no fixed `Username/Password/...` vocabulary.
//!
//! The derive emits an inherent `auth_methods()` plus `TryFrom<Credential>`, so a
//! backend matches an exhaustive, typed enum instead of poking at a loose field
//! list.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Field, Fields, LitStr, Type, Variant};

#[proc_macro_derive(CredentialSchema, attributes(credential))]
pub fn derive_credential_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_credential_schema(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_credential_schema(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = input.ident;
    let Data::Enum(data) = input.data else {
        return Err(syn::Error::new_spanned(
            &name,
            "CredentialSchema can only be derived for an enum",
        ));
    };

    // One `AuthMethod` literal per form-method variant, and one parse block per
    // variant (form methods *and* restore-only shapes).
    let mut methods = Vec::new();
    let mut parse_blocks = Vec::new();

    for variant in data.variants {
        let vmeta = VariantMeta::parse(&variant)?;
        let vident = variant.ident.clone();

        let Fields::Named(named) = &variant.fields else {
            return Err(syn::Error::new_spanned(
                &variant,
                "each credential variant must use named fields",
            ));
        };

        let mut specs = Vec::new();
        let mut decls = Vec::new();
        let mut arms = Vec::new();
        let mut required = Vec::new();
        let mut construct = Vec::new();

        for field in &named.named {
            let fident = field.ident.clone().unwrap();
            let key = fident.to_string();
            let optional = is_option(&field.ty);
            let fmeta = FieldMeta::parse(field)?;

            // Field prompt metadata. A rendered method must label every field; a
            // hidden method displays nothing, so its label is optional and only
            // the `key` it carries is meaningful.
            let label = match (vmeta.hidden, fmeta.label.clone()) {
                (_, Some(label)) => quote! { #label },
                (true, None) => quote! { "" },
                (false, None) => {
                    return Err(syn::Error::new_spanned(
                        field,
                        "a form-method field needs `#[credential(label = \"...\")]`",
                    ));
                }
            };
            let secret = fmeta.secret;
            specs.push(quote! {
                ::messenger_interface::auth::CredentialFieldSpec {
                    key: #key,
                    label: #label,
                    secret: #secret,
                    optional: #optional,
                }
            });

            // Parse — collect this field's value out of the erased credential.
            decls.push(quote! { let mut #fident = ::core::option::Option::None; });
            arms.push(quote! {
                #key => {
                    if #fident.is_none() {
                        #fident = ::core::option::Option::Some(__field.value.clone());
                    }
                }
            });
            if !optional {
                required.push(fident.clone());
            }
            construct.push(quote! { #fident });
        }

        // Every variant becomes an `AuthMethod` so `auth_methods()` is the
        // complete key vocabulary; `hidden` ones are simply not rendered.
        let method_label = match &vmeta.label {
            Some(label) => quote! { #label },
            None => quote! { "" },
        };
        let hidden = vmeta.hidden;
        methods.push(quote! {
            ::messenger_interface::auth::AuthMethod {
                label: #method_label,
                hidden: #hidden,
                fields: &[ #(#specs),* ],
            }
        });

        // A variant matches when every required field was supplied; optional
        // fields pass through as `Option`. First matching variant wins.
        let success = quote! {
            return ::core::result::Result::Ok(#name::#vident { #(#construct),* });
        };
        let body = if required.is_empty() {
            success
        } else {
            quote! {
                if let ( #( ::core::option::Option::Some(#required) ),* ,) =
                    ( #( #required ),* ,)
                {
                    #success
                }
            }
        };

        parse_blocks.push(quote! {
            {
                #(#decls)*
                for __field in &__cred.0 {
                    match __field.key {
                        #(#arms)*
                        _ => {}
                    }
                }
                #body
            }
        });
    }

    Ok(quote! {
        impl #name {
            /// The selectable login methods this credential exposes.
            pub fn auth_methods() -> &'static [::messenger_interface::auth::AuthMethod] {
                const __METHODS: &[::messenger_interface::auth::AuthMethod] =
                    &[ #(#methods),* ];
                __METHODS
            }
        }

        impl ::core::convert::TryFrom<::messenger_interface::auth::Credential> for #name {
            type Error = ::messenger_interface::auth::CredentialError;
            fn try_from(
                __cred: ::messenger_interface::auth::Credential,
            ) -> ::core::result::Result<Self, Self::Error> {
                #(#parse_blocks)*
                ::core::result::Result::Err(
                    ::messenger_interface::auth::CredentialError::Unrecognized,
                )
            }
        }
    })
}

/// `#[credential(method = "...")]` xor `#[credential(hidden)]` on a variant. A
/// `method` is a login shape the UI renders (carrying its display label); a
/// `hidden` shape is still parseable (e.g. a session reconstructed from storage)
/// but never rendered, so it needs no label.
struct VariantMeta {
    label: Option<LitStr>,
    hidden: bool,
}

impl VariantMeta {
    fn parse(variant: &Variant) -> syn::Result<Self> {
        let mut label = None;
        let mut hidden = false;
        for attr in &variant.attrs {
            if !attr.path().is_ident("credential") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("method") {
                    label = Some(meta.value()?.parse::<LitStr>()?);
                    Ok(())
                } else if meta.path.is_ident("hidden") {
                    hidden = true;
                    Ok(())
                } else {
                    Err(meta.error("expected `method = \"...\"` or `hidden`"))
                }
            })?;
        }
        match (label.is_some(), hidden) {
            (true, false) | (false, true) => Ok(Self { label, hidden }),
            (true, true) => Err(syn::Error::new_spanned(
                variant,
                "a variant cannot be both `method` and `hidden`",
            )),
            (false, false) => Err(syn::Error::new_spanned(
                variant,
                "a variant needs `#[credential(method = \"...\")]` or `#[credential(hidden)]`",
            )),
        }
    }
}

/// `#[credential(label = "...", secret)]` on a field.
struct FieldMeta {
    label: Option<LitStr>,
    secret: bool,
}

impl FieldMeta {
    fn parse(field: &Field) -> syn::Result<Self> {
        let mut label = None;
        let mut secret = false;
        for attr in &field.attrs {
            if !attr.path().is_ident("credential") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("label") {
                    label = Some(meta.value()?.parse::<LitStr>()?);
                    Ok(())
                } else if meta.path.is_ident("secret") {
                    secret = true;
                    Ok(())
                } else {
                    Err(meta.error("expected `label = \"...\"` or `secret`"))
                }
            })?;
        }
        Ok(Self { label, secret })
    }
}

/// Whether a field type is spelled `Option<_>` (⇒ an optional credential field).
fn is_option(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.path.segments.last().is_some_and(|seg| seg.ident == "Option"))
}

