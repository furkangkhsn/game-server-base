//! `#[service]`: turns a plain async trait into a gsb service
//! (SERVICES.md §4). In the engine this is `#[gsb::service]`; here the
//! generated paths point at the spike crate (`::s0_calls`).
//!
//! From
//!
//! ```ignore
//! #[service]
//! pub trait Inventory {
//!     async fn load(&self, cx: &CallCx, who: Identity) -> Result<InventoryData, InvError>;
//! }
//! ```
//!
//! it generates, next to the (rewritten, `Send`-future) trait:
//!
//! - `mod inventory`: one call type per method (`inventory::Load { who }`
//!   with `impl Call`), the mailbox enum `Msg`, the marker `Kind`
//!   (`ServiceKind` and `ServiceDef<S>` for every `S: Inventory`) and
//!   the tower router `Router<S>`;
//! - `InventoryClient`: the NON-blocking client for rooms and other tick
//!   actors (`inv.load(who)` builds a `CallSpec` for `calls.call(..)`);
//! - `InventoryHandle`: the awaitable client for services, admin and boot
//!   code (`inv.load(who).await`);
//! - `Inventory::serve(impl)`: the value a host registers.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    FnArg, GenericArgument, Ident, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type,
    parse_macro_input, spanned::Spanned,
};

struct Method {
    attrs: Vec<syn::Attribute>,
    name: Ident,
    call: Ident,
    cx: syn::PatType,
    args: Vec<(Ident, Type)>,
    ok: Type,
    err: Type,
}

#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(Span::call_site(), "#[service] takes no arguments")
            .to_compile_error()
            .into();
    }
    let tr = parse_macro_input!(item as ItemTrait);
    match expand(tr) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(tr: ItemTrait) -> syn::Result<TokenStream2> {
    if !tr.generics.params.is_empty() {
        return Err(syn::Error::new(
            tr.generics.span(),
            "a service trait has no generics",
        ));
    }
    let vis = &tr.vis;
    let name = &tr.ident;
    let trait_attrs = &tr.attrs;
    let module = format_ident!("{}", snake(&name.to_string()));
    let client = format_ident!("{}Client", name);
    let handle = format_ident!("{}Handle", name);
    let name_str = name.to_string();

    let mut methods = Vec::new();
    for item in &tr.items {
        let TraitItem::Fn(f) = item else {
            return Err(syn::Error::new(
                item.span(),
                "a service trait holds only `async fn`s",
            ));
        };
        methods.push(method(f)?);
    }

    let krate = quote!(::s0_calls);

    let trait_fns = methods.iter().map(|m| {
        let Method {
            attrs,
            name,
            cx,
            args,
            ok,
            err,
            ..
        } = m;
        let arg_list = args.iter().map(|(a, t)| quote!(#a: #t));
        quote! {
            #(#attrs)*
            fn #name(&self, #cx, #(#arg_list),*)
                -> impl ::core::future::Future<Output = ::core::result::Result<#ok, #err>> + ::core::marker::Send;
        }
    });

    let call_types = methods.iter().map(|m| {
        let Method {
            call,
            args,
            ok,
            err,
            ..
        } = m;
        let fields = args.iter().map(|(a, t)| quote!(pub #a: #t));
        quote! {
            /// One call of the method of the same name (generated).
            #[derive(Clone)]
            pub struct #call { #(#fields),* }
            impl #krate::Call for #call {
                type Response = #ok;
                type Error = #err;
            }
        }
    });

    let msg_arms = methods.iter().map(|m| {
        let call = &m.call;
        quote!(#call(#call))
    });

    let dispatch_arms = methods.iter().map(|m| {
        let Method {
            name, call, args, ..
        } = m;
        let fields: Vec<_> = args.iter().map(|(a, _)| a).collect();
        quote! {
            Msg::#call(#call { #(#fields),* }) => {
                let out: #krate::Outcome<#call> = svc
                    .#name(&cx, #(#fields),*)
                    .await
                    .map_err(#krate::CallError::Service);
                ::std::boxed::Box::new(out)
            }
        }
    });

    let client_fns = methods.iter().map(|m| {
        let Method {
            attrs,
            name,
            call,
            args,
            ..
        } = m;
        let params = args.iter().map(|(a, t)| quote!(#a: #t));
        let fields = args.iter().map(|(a, _)| a);
        quote! {
            #(#attrs)*
            /// Builds the call; hand it to `calls.call(..)`. Never blocks.
            pub fn #name(&self, #(#params),*) -> #krate::CallSpec<#module::#call> {
                #krate::CallSpec::new(&self.route, #module::#call { #(#fields),* }, #module::Msg::#call)
            }
        }
    });

    let handle_fns = methods.iter().map(|m| {
        let Method {
            attrs,
            name,
            call,
            args,
            ..
        } = m;
        let params = args.iter().map(|(a, t)| quote!(#a: #t));
        let fields = args.iter().map(|(a, _)| a);
        quote! {
            #(#attrs)*
            /// Calls and awaits the answer (services, admin, boot code).
            pub async fn #name(&self, #(#params),*) -> #krate::Outcome<#module::#call> {
                self.route
                    .call_await(#module::#call { #(#fields),* }, #module::Msg::#call, self.player.clone())
                    .await
            }
        }
    });

    Ok(quote! {
        #(#trait_attrs)*
        #vis trait #name: ::core::marker::Send + ::core::marker::Sync + 'static {
            #(#trait_fns)*

            /// The value a host registers:
            #[doc = concat!("`server.service(", #name_str, "::serve(my_impl), |s| s)`.")]
            fn serve(self) -> #krate::Served<#module::Kind, Self>
            where
                Self: ::core::marker::Sized,
            {
                #krate::Served::new(self)
            }
        }

        #[doc = concat!("Generated by `#[service]` for [`", #name_str, "`].")]
        #[allow(unused_imports)]
        #vis mod #module {
            use super::*;

            #(#call_types)*

            /// The service's mailbox message (generated).
            pub enum Msg { #(#msg_arms),* }

            /// The service marker: names the mailbox type, builds the router.
            pub struct Kind;

            impl #krate::ServiceKind for Kind {
                const NAME: &'static str = #name_str;
                type Msg = Msg;
            }

            impl<__S: super::#name> #krate::ServiceDef<__S> for Kind {
                type Router = Router<__S>;
                fn router(svc: ::std::sync::Arc<__S>) -> Router<__S> {
                    Router(svc)
                }
            }

            /// The tower router: one `Dispatch<Msg>` in, the erased outcome out.
            pub struct Router<__S>(::std::sync::Arc<__S>);

            impl<__S> ::core::clone::Clone for Router<__S> {
                fn clone(&self) -> Self {
                    Router(::std::sync::Arc::clone(&self.0))
                }
            }

            impl<__S: super::#name> #krate::__private::tower::Service<#krate::Dispatch<Msg>> for Router<__S> {
                type Response = #krate::Erased;
                type Error = #krate::__private::BoxError;
                type Future = #krate::__private::BoxFuture;

                fn poll_ready(
                    &mut self,
                    _: &mut ::core::task::Context<'_>,
                ) -> ::core::task::Poll<::core::result::Result<(), Self::Error>> {
                    ::core::task::Poll::Ready(::core::result::Result::Ok(()))
                }

                fn call(&mut self, d: #krate::Dispatch<Msg>) -> Self::Future {
                    let svc = ::std::sync::Arc::clone(&self.0);
                    ::std::boxed::Box::pin(async move {
                        let #krate::Dispatch { cx, msg } = d;
                        let erased: #krate::Erased = match msg {
                            #(#dispatch_arms)*
                        };
                        ::core::result::Result::Ok(erased)
                    })
                }
            }
        }

        #[doc = concat!("Non-blocking client of [`", #name_str, "`] for tick actors (generated).")]
        #[derive(Clone)]
        #vis struct #client {
            route: #krate::Route<#module::Msg>,
        }

        impl #client {
            #(#client_fns)*
        }

        impl #krate::ServiceClient for #client {
            type Kind = #module::Kind;
            fn from_route(route: #krate::Route<#module::Msg>) -> Self {
                Self { route }
            }
        }

        #[doc = concat!("Awaitable client of [`", #name_str, "`] for services and admin code (generated).")]
        #[derive(Clone)]
        #vis struct #handle {
            route: #krate::Route<#module::Msg>,
            player: ::core::option::Option<#krate::Identity>,
        }

        impl #handle {
            /// Calls made from inside a service call act for the same
            /// verified player (`cx.player()` propagates).
            pub fn within(&self, cx: &#krate::CallCx) -> Self {
                Self { route: self.route.clone(), player: cx.player().cloned() }
            }

            /// Calls act for this verified player (admin, boot code).
            pub fn on_behalf_of(&self, who: &#krate::Identity) -> Self {
                Self { route: self.route.clone(), player: ::core::option::Option::Some(who.clone()) }
            }

            #(#handle_fns)*
        }

        impl #krate::ServiceClient for #handle {
            type Kind = #module::Kind;
            fn from_route(route: #krate::Route<#module::Msg>) -> Self {
                Self { route, player: ::core::option::Option::None }
            }
        }
    })
}

fn method(f: &syn::TraitItemFn) -> syn::Result<Method> {
    let sig = &f.sig;
    if sig.asyncness.is_none() {
        return Err(syn::Error::new(
            sig.span(),
            "service methods are `async fn`",
        ));
    }
    if f.default.is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "service methods have no default body",
        ));
    }
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "service methods have no generics",
        ));
    }
    let mut inputs = sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
        _ => {
            return Err(syn::Error::new(
                sig.span(),
                "the first parameter is `&self`",
            ));
        }
    }
    let cx = match inputs.next() {
        Some(FnArg::Typed(p)) => p.clone(),
        _ => {
            return Err(syn::Error::new(
                sig.span(),
                "the second parameter is `cx: &CallCx`",
            ));
        }
    };
    let mut args = Vec::new();
    for input in inputs {
        let FnArg::Typed(p) = input else {
            return Err(syn::Error::new(input.span(), "unexpected receiver"));
        };
        let Pat::Ident(id) = &*p.pat else {
            return Err(syn::Error::new(
                p.pat.span(),
                "service arguments are plain names",
            ));
        };
        args.push((id.ident.clone(), (*p.ty).clone()));
    }
    let (ok, err) = result_types(&sig.output)?;
    Ok(Method {
        attrs: f.attrs.clone(),
        name: sig.ident.clone(),
        call: format_ident!("{}", camel(&sig.ident.to_string())),
        cx,
        args,
        ok,
        err,
    })
}

fn result_types(out: &ReturnType) -> syn::Result<(Type, Type)> {
    let err = || {
        syn::Error::new(
            out.span(),
            "service methods return `Result<Response, Error>`",
        )
    };
    let ReturnType::Type(_, ty) = out else {
        return Err(err());
    };
    let Type::Path(p) = &**ty else {
        return Err(err());
    };
    let last = p.path.segments.last().ok_or_else(err)?;
    if last.ident != "Result" {
        return Err(err());
    }
    let PathArguments::AngleBracketed(a) = &last.arguments else {
        return Err(err());
    };
    let mut tys = a.args.iter().filter_map(|g| match g {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    match (tys.next(), tys.next(), tys.next()) {
        (Some(ok), Some(e), None) => Ok((ok, e)),
        _ => Err(err()),
    }
}

fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn camel(s: &str) -> String {
    s.split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}
