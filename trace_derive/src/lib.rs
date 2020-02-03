extern crate proc_macro;

use crate::proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Error, Field, Lit, Meta, MetaNameValue, Path};

#[proc_macro_derive(Trace, attributes(trace))]
pub fn traceable_derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let generics = input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(s) => &s.fields,
        _ => panic!("Expected a struct"),
    };

    let fields: Result<Vec<_>, Error> = fields.iter().map(|field| get_trace(field)).collect();

    let (masked, unmasked): (Vec<_>, Vec<_>) = fields
        .unwrap()
        .into_iter()
        .filter_map(|x| x)
        .partition(|x| x.1.is_some());

    // I've not worked out how to split a tuple during quote! interpolation.
    // Until I'm aware of a cleaner way, we're stuck with the boilerplate below.
    let unmasked_name = unmasked.iter().map(|x| &x.0.ident);
    let masked_name = masked.iter().map(|x| &x.0.ident);
    let masked_fn = masked.iter().map(|x| &x.1);

    fn get_trace(field: &Field) -> Result<Option<(&Field, Option<Path>)>, Error> {
        for attr in field.attrs.iter() {
            if !attr.path.is_ident("trace") {
                continue;
            }

            match attr.parse_meta()? {
                Meta::Word(_) => return Ok(Some((&field, None))),
                Meta::NameValue(MetaNameValue {
                    lit: Lit::Str(lit_str),
                    ..
                }) => return lit_str.parse().map(|x| Some((field, Some(x)))),
                _ => {
                    let message = "expected #[trace = \"...\"]";
                    return Err(Error::new_spanned(attr, message));
                }
            }
        }
        return Ok(None);
    }

    let expanded = quote! {
        impl #impl_generics Trace for #name #ty_generics #where_clause {
            fn trace(&self, current: &mut Vec<usize>) -> TraceType {
                    #(
                        current.push(self.#unmasked_name as *const _ as usize);
                     )*

                    #(
                        let ptr: *const u8 = #masked_fn(self.#masked_name);
                        current.push(ptr as usize);
                     )*

                    TraceType::Custom
            }
        }
    };

    TokenStream::from(expanded)
}
