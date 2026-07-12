//! `#[derive(AfxParams)]` — generates an effect's parameter surface from its
//! annotated fields.
//!
//! A field tagged `#[afx(param(name = ..., unit = ..., min = ..., max = ...,
//! default = ..., min_is_off = ...))]` becomes one UI-tunable parameter: the
//! derive collects every tagged field into a static `AFX_PARAMS` spec table,
//! emits one `ParamKey` constant per parameter (the field name uppercased),
//! and implements the `AfxParams` trait — `param_specs`, `param_value`, and a
//! range-clamping `set_param`. A struct-level `#[afx(after_set = path)]` names
//! a method to run after any successful write, for effects that resolve their
//! fields into derived state.
//!
//! The generated impl references items by `crate::params::...` paths, so the
//! derive only works inside the audio crate that defines that module; it is
//! re-exported there next to the trait.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Expr, Field, Fields, Path, Type};

#[proc_macro_derive(AfxParams, attributes(afx))]
pub fn derive_afx_params(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_afx_params(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_afx_params(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = input.ident;
    let meta = AfxTypeMeta::parse(&input.attrs)?;
    let Data::Struct(data) = input.data else {
        return Err(syn::Error::new_spanned(
            &name,
            "AfxParams can only be derived for a struct",
        ));
    };

    let fields = match data.fields {
        Fields::Named(named) => named.named,
        Fields::Unnamed(_) | Fields::Unit => {
            return Err(syn::Error::new_spanned(
                &name,
                "AfxParams needs a struct with named fields",
            ));
        }
    };

    let mut specs = Vec::new();
    let mut key_consts = Vec::new();
    let mut value_arms = Vec::new();
    let mut set_arms = Vec::new();

    for field in fields {
        let Some(meta) = AfxFieldMeta::parse(&field)? else {
            continue;
        };
        if !is_f32(&field.ty) {
            return Err(syn::Error::new_spanned(
                &field.ty,
                "afx params must be f32 fields",
            ));
        }
        let field_name = field.ident.clone().unwrap();
        let index = specs.len();
        let key_const = syn::Ident::new(&field_name.to_string().to_uppercase(), field_name.span());
        let key_doc =
            format!("[`ParamKey`](crate::params::ParamKey) of the `{field_name}` parameter.");
        key_consts.push(quote! {
            #[doc = #key_doc]
            pub const #key_const: crate::params::ParamKey = crate::params::ParamKey(#index);
        });
        let AfxFieldMeta {
            name: param_name,
            unit,
            min,
            max,
            default,
            min_is_off,
        } = meta;

        specs.push(quote! {
            crate::params::ParamSpec {
                key: crate::params::ParamKey(#index),
                name: #param_name,
                unit: #unit,
                min: #min,
                max: #max,
                default: #default,
                min_is_off: #min_is_off,
            }
        });
        value_arms.push(quote! {
            #index => ::core::option::Option::Some(self.#field_name)
        });
        set_arms.push(quote! {
            #index => {
                self.#field_name = value.clamp(#min, #max);
                true
            }
        });
    }

    let after_set = meta.after_set.map(|hook| {
        quote! {
            if __wrote {
                self.#hook();
            }
        }
    });

    Ok(quote! {
        impl #name {
            #(#key_consts)*

            const AFX_PARAMS: &'static [crate::params::ParamSpec] = &[
                #(#specs),*
            ];
        }

        impl crate::params::AfxParams for #name {
            fn param_specs(&self) -> &'static [crate::params::ParamSpec] {
                Self::AFX_PARAMS
            }

            fn param_value(
                &self,
                key: crate::params::ParamKey,
            ) -> ::core::option::Option<f32> {
                match key.0 {
                    #(#value_arms,)*
                    _ => ::core::option::Option::None,
                }
            }

            fn set_param(&mut self, key: crate::params::ParamKey, value: f32) -> bool {
                let __wrote = match key.0 {
                    #(#set_arms,)*
                    _ => false,
                };
                #after_set
                __wrote
            }
        }
    })
}

struct AfxTypeMeta {
    after_set: Option<Path>,
}

impl AfxTypeMeta {
    fn parse(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut after_set = None;
        for attr in attrs {
            if !attr.path().is_ident("afx") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("after_set") {
                    after_set = Some(meta.value()?.parse::<Path>()?);
                    Ok(())
                } else {
                    Err(meta.error("expected `after_set = path`"))
                }
            })?;
        }
        Ok(Self { after_set })
    }
}

struct AfxFieldMeta {
    name: Expr,
    unit: Expr,
    min: Expr,
    max: Expr,
    default: Expr,
    min_is_off: Expr,
}

impl AfxFieldMeta {
    fn parse(field: &Field) -> syn::Result<Option<Self>> {
        let mut parsed = None;
        for attr in &field.attrs {
            if !attr.path().is_ident("afx") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if !meta.path.is_ident("param") {
                    return Err(meta.error("expected `param(...)`"));
                }
                if parsed.is_some() {
                    return Err(meta.error("duplicate afx param"));
                }

                let mut name = None;
                let mut unit = None;
                let mut min = None;
                let mut max = None;
                let mut default = None;
                let mut min_is_off = None;

                meta.parse_nested_meta(|param| {
                    if param.path.is_ident("name") {
                        name = Some(param.value()?.parse::<Expr>()?);
                    } else if param.path.is_ident("unit") {
                        unit = Some(param.value()?.parse::<Expr>()?);
                    } else if param.path.is_ident("min") {
                        min = Some(param.value()?.parse::<Expr>()?);
                    } else if param.path.is_ident("max") {
                        max = Some(param.value()?.parse::<Expr>()?);
                    } else if param.path.is_ident("default") {
                        default = Some(param.value()?.parse::<Expr>()?);
                    } else if param.path.is_ident("min_is_off") {
                        min_is_off = Some(param.value()?.parse::<Expr>()?);
                    } else {
                        return Err(param.error(
                            "expected `name`, `unit`, `min`, `max`, `default`, or `min_is_off`",
                        ));
                    }
                    Ok(())
                })?;

                parsed = Some(Self {
                    name: required_afx_field(field, name, "name")?,
                    unit: required_afx_field(field, unit, "unit")?,
                    min: required_afx_field(field, min, "min")?,
                    max: required_afx_field(field, max, "max")?,
                    default: required_afx_field(field, default, "default")?,
                    min_is_off: required_afx_field(field, min_is_off, "min_is_off")?,
                });
                Ok(())
            })?;
        }
        Ok(parsed)
    }
}

fn required_afx_field<T>(field: &Field, value: Option<T>, name: &str) -> syn::Result<T> {
    value.ok_or_else(|| {
        syn::Error::new_spanned(field, format!("afx param missing required `{name} = ...`"))
    })
}

fn is_f32(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.path.segments.len() == 1 && tp.path.segments[0].ident == "f32")
}
