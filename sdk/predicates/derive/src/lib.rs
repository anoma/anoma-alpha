use {
  proc_macro::TokenStream,
  quote::quote,
  syn::{parse_macro_input, parse_quote, Abi, FnArg, ItemFn, ReturnType, Type},
};

#[proc_macro_attribute]
pub fn predicate(_: TokenStream, item: TokenStream) -> TokenStream {
  let mut input_fn = parse_macro_input!(item as ItemFn);

  if !verify_signature(&input_fn) {
    panic!(
      "Expecting predicates to be a function with the following signature: \
       fn(&[Param], &Transaction) -> bool"
    );
  }

  decorate_entrypoint_abi(&mut input_fn);

  TokenStream::from(quote!(#input_fn))
}

fn verify_signature(input_fn: &ItemFn) -> bool {
  let mut argiter = input_fn.sig.inputs.iter();
  let first = argiter.next();
  let second = argiter.next();
  let ret = &input_fn.sig.output;

  let args_ok = match (first, second, input_fn.sig.inputs.len()) {
    (Some(FnArg::Typed(first)), Some(FnArg::Typed(second)), 2) => {
      let mut args = (false, false);
      if let Type::Reference(ref reftype) = *first.ty {
        if reftype.mutability.is_none() {
          if let Type::Slice(ref slice) = *reftype.elem {
            if let Type::Path(ref path) = *slice.elem {
              if let Some(ident) = path.path.segments.last() {
                if ident.ident == "Param" {
                  args.0 = true;
                }
              }
            }
          }
        }
      }

      if let Type::Reference(ref reftype) = *second.ty {
        if reftype.mutability.is_none() {
          if let Type::Path(ref path) = *reftype.elem {
            if let Some(elem) = path.path.segments.last() {
              if elem.ident == "Transaction" {
                args.1 = true;
              }
            }
          }
        }
      }
      args.0 && args.1
    }
    _ => return false,
  };

  if !args_ok {
    return false;
  }

  if let ReturnType::Type(_, ty) = ret {
    if let Type::Path(ref path) = **ty {
      if !path.path.is_ident("bool") {
        return false;
      }
    }
  }

  true
}

/// Adds `no_mangle` attribute and pub extern "C"
/// so the predicate is exported by WASM
fn decorate_entrypoint_abi(input_fn: &mut ItemFn) {
  input_fn.attrs.push(parse_quote! {
    #[no_mangle]
  });
  input_fn.attrs.push(parse_quote! {
    #[allow(improper_ctypes_definitions)]
  });

  input_fn.sig.abi = Some(Abi {
    extern_token: parse_quote!(extern),
    name: None,
  });
}

#[proc_macro]
pub fn initialize_library(_: TokenStream) -> TokenStream {
  TokenStream::from(quote! {})
}
