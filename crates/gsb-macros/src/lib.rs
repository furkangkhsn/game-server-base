//! `#[service]`: turns a plain async trait into a gsb in-process service
//! (`docs/SERVICES.md` §4.1). Use it through the engine's re-export,
//! `#[gsb_core::service]`.
//!
//! From
//!
//! ```ignore
//! #[gsb_core::service]
//! pub trait Inventory {
//!     async fn load(&self, cx: &CallCx, who: Identity) -> Result<InventoryData, InvError>;
//! }
//! ```
//!
//! it generates, next to the trait (rewritten so every method returns a
//! `Send` future — RPITIT, no `async-trait`):
//!
//! - `mod inventory`: one call type per method (`inventory::Load { who }`
//!   with `impl Call`), the mailbox message `Msg` (its variants carry the
//!   calls' `Ask`s), the bare message `Bare` the tower stack sees, the
//!   marker `Kind` (`ServiceKind`, and `ServiceDef<S>` for every
//!   `S: Inventory`) and the tower router `Router<S>`;
//! - `InventoryClient`: the NON-blocking client for tick actors
//!   (`inv.load(who)` builds a `CallSpec` for `calls.call(..)`);
//! - `InventoryHandle`: the awaitable client for services, admin and boot
//!   code (`inv.load(who).await`, `within(cx)`, `on_behalf_of(&id)`);
//! - `Inventory::serve(impl)`: the value a host registers.
//!
//! **Engine events** (`docs/EVENTS.md` §5.4). One method may be marked
//! `#[gsb(engine_events)]`: `async fn on_engine(&self, e: EngineEvent)`
//! (or `e: EngineNotice`, to see the subscriber's sequence numbers) — no
//! `cx`, no result. It is no call (no call type, no client method): the
//! host runs it for every engine event the registration subscribes to
//! (`Setup::engine_events`). At most one per trait; a trait may hold
//! only it.
//!
//! **The engine path.** Generated code names the engine crate as
//! `::gsb_core`. A crate that reaches the engine under another path
//! (a re-export) says so: `#[gsb_core::service(crate = my::engine)]`.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    FnArg, GenericArgument, Ident, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type,
    parse_macro_input, spanned::Spanned,
};

/// The names the generated module and clients define themselves: a
/// method whose call type or name would collide is refused.
const RESERVED_CALLS: &[&str] = &["Msg", "Bare", "Kind", "Router"];
const RESERVED_METHODS: &[&str] = &["serve", "within", "on_behalf_of", "route", "player"];

/// The `#[gsb(engine_events)]` method.
struct EngineMethod {
    attrs: Vec<syn::Attribute>,
    name: Ident,
    arg: Ident,
    ty: Type,
}

struct Method {
    docs: Vec<syn::Attribute>,
    attrs: Vec<syn::Attribute>,
    name: Ident,
    call: Ident,
    cx: syn::PatType,
    args: Vec<(Ident, Type)>,
    ok: Type,
    err: Type,
}

/// `crate = path`, or nothing.
struct Args {
    krate: Option<syn::Path>,
}

impl Parse for Args {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self { krate: None });
        }
        let key: syn::Token![crate] = input.parse()?;
        let _ = key;
        let _: syn::Token![=] = input.parse()?;
        let path: syn::Path = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("#[service] takes at most `crate = path`"));
        }
        Ok(Self { krate: Some(path) })
    }
}

/// Turn a plain async trait into a gsb service (see the crate docs).
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as Args);
    let tr = parse_macro_input!(item as ItemTrait);
    match expand(args, tr) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(args: Args, tr: ItemTrait) -> syn::Result<TokenStream2> {
    if !tr.generics.params.is_empty() || tr.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            tr.generics.span(),
            "a service trait has no generics",
        ));
    }
    if !tr.supertraits.is_empty() {
        return Err(syn::Error::new(
            tr.supertraits.span(),
            "a service trait has no supertraits (it is `Send + Sync + 'static` already)",
        ));
    }
    if tr.unsafety.is_some() || tr.auto_token.is_some() {
        return Err(syn::Error::new(
            tr.span(),
            "a service trait is a plain trait",
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
    let mut engine: Option<EngineMethod> = None;
    for item in &tr.items {
        let TraitItem::Fn(f) = item else {
            return Err(syn::Error::new(
                item.span(),
                "a service trait holds only `async fn`s",
            ));
        };
        if is_engine_events(f)? {
            if engine.is_some() {
                return Err(syn::Error::new(
                    f.sig.ident.span(),
                    "a service has at most one `#[gsb(engine_events)]` method",
                ));
            }
            engine = Some(engine_method(f)?);
        } else {
            methods.push(method(f)?);
        }
    }
    if methods.is_empty() && engine.is_none() {
        return Err(syn::Error::new(
            name.span(),
            "a service has at least one method",
        ));
    }

    let krate = match &args.krate {
        Some(p) => quote!(#p),
        None => quote!(::gsb_core),
    };
    let calls = quote!(#krate::calls);

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

    let engine_trait_fn = engine.as_ref().map(|e| {
        let EngineMethod {
            attrs,
            name,
            arg,
            ty,
        } = e;
        quote! {
            #(#attrs)*
            fn #name(&self, #arg: #ty)
                -> impl ::core::future::Future<Output = ()> + ::core::marker::Send;
        }
    });
    let engine_def = engine.as_ref().map(|e| {
        let m = &e.name;
        quote! {
            fn engine_events(
                svc: &::std::sync::Arc<__S>,
            ) -> ::core::option::Option<#calls::EngineHandler> {
                let svc = ::std::sync::Arc::clone(svc);
                ::core::option::Option::Some(::std::boxed::Box::new(
                    move |n: #krate::events::EngineNotice| -> #calls::EngineFuture {
                        let svc = ::std::sync::Arc::clone(&svc);
                        ::std::boxed::Box::pin(async move {
                            svc.#m(::core::convert::Into::into(n)).await
                        })
                    },
                ))
            }
        }
    });

    let call_types = methods.iter().map(|m| {
        let Method {
            name,
            call,
            args,
            ok,
            err,
            ..
        } = m;
        let fields = args.iter().map(|(a, t)| quote!(pub #a: #t));
        let doc = format!("One call of `{name_str}::{name}` (generated).");
        quote! {
            #[doc = #doc]
            #[derive(Clone)]
            pub struct #call { #(#fields),* }
            impl #calls::Call for #call {
                type Response = #ok;
                type Error = #err;
            }
        }
    });

    let msg_arms = methods.iter().map(|m| {
        let call = &m.call;
        quote!(#call(#calls::Ask<#call>))
    });
    let bare_arms = methods.iter().map(|m| {
        let call = &m.call;
        quote!(#call(#call))
    });
    let split_arms = methods.iter().map(|m| {
        let call = &m.call;
        quote! {
            Msg::#call(ask) => {
                let (call, reply) = ask.split();
                (Bare::#call(call), reply)
            }
        }
    });
    let method_names = methods.iter().map(|m| m.name.to_string());
    let call_names = methods.iter().map(|m| &m.call);
    let index_arms = methods.iter().enumerate().map(|(i, m)| {
        let call = &m.call;
        quote!(Bare::#call(_) => #i)
    });
    // A trait with no call method: the method index matches nothing.
    let method_body = if methods.is_empty() {
        quote!(match *bare {})
    } else {
        quote!(match bare { #(#index_arms),* })
    };

    let dispatch_arms = methods.iter().map(|m| {
        let Method {
            name, call, args, ..
        } = m;
        let fields: Vec<_> = args.iter().map(|(a, _)| a).collect();
        quote! {
            Bare::#call(#call { #(#fields),* }) => {
                let out: #calls::Outcome<#call> = svc
                    .#name(&cx, #(#fields),*)
                    .await
                    .map_err(#calls::CallError::Service);
                ::std::boxed::Box::new(out)
            }
        }
    });

    let client_fns = methods.iter().map(|m| {
        let Method {
            docs,
            name,
            call,
            args,
            ..
        } = m;
        let params = args.iter().map(|(a, t)| quote!(#a: #t));
        let fields = args.iter().map(|(a, _)| a);
        quote! {
            #(#docs)*
            ///
            /// Builds the call (generated): hand it to `calls.call(..)`.
            /// Never blocks.
            pub fn #name(&self, #(#params),*) -> #calls::CallSpec<#module::#call> {
                #calls::CallSpec::new(&self.route, #module::#call { #(#fields),* }, #module::Msg::#call)
            }
        }
    });

    let handle_fns = methods.iter().map(|m| {
        let Method {
            docs,
            name,
            call,
            args,
            ..
        } = m;
        let params = args.iter().map(|(a, t)| quote!(#a: #t));
        let fields = args.iter().map(|(a, _)| a);
        quote! {
            #(#docs)*
            ///
            /// Calls and awaits the answer (generated; services, admin,
            /// boot code). A full or gone mailbox ends it at once
            /// (`Refused`).
            pub async fn #name(&self, #(#params),*) -> #calls::Outcome<#module::#call> {
                self.route
                    .ask(#module::#call { #(#fields),* }, #module::Msg::#call, self.on.clone())
                    .await
            }
        }
    });

    let serve_doc = format!(
        "The value a host registers (generated): `parts.service({name_str}::serve(my_impl), |s| s)`."
    );
    let mod_doc = format!("Generated by `#[service]` for [`{name_str}`].");
    let client_doc = format!(
        "Non-blocking client of [`{name_str}`] for tick actors (generated): each method builds a `CallSpec` for `calls.call(..)`."
    );
    let handle_doc = format!(
        "Awaitable client of [`{name_str}`] for services, admin and boot code (generated)."
    );

    Ok(quote! {
        #(#trait_attrs)*
        #vis trait #name: ::core::marker::Send + ::core::marker::Sync + 'static {
            #(#trait_fns)*

            #engine_trait_fn

            #[doc = #serve_doc]
            fn serve(self) -> #calls::Served<#module::Kind, Self>
            where
                Self: ::core::marker::Sized,
            {
                #calls::Served::new(self)
            }
        }

        #[doc = #mod_doc]
        #[allow(unused_imports, unused_variables, missing_docs, clippy::enum_variant_names)]
        #vis mod #module {
            use super::*;

            #(#call_types)*

            /// The service's mailbox message (generated): one variant per
            /// method, each carrying the call's `Ask`.
            pub enum Msg { #(#msg_arms),* }

            /// The bare message the tower stack sees (generated): the
            /// call without its answer right (the host keeps that).
            pub enum Bare { #(#bare_arms),* }

            /// The service marker (generated): names the messages,
            /// builds the router.
            pub struct Kind;

            impl #calls::ServiceKind for Kind {
                const NAME: &'static str = #name_str;
                const METHODS: &'static [&'static str] = &[#(#method_names),*];
                type Msg = Msg;
                type Bare = Bare;
                fn split(msg: Msg) -> (Bare, #calls::Reply) {
                    match msg {
                        #(#split_arms)*
                    }
                }
                fn method(bare: &Bare) -> usize {
                    #method_body
                }
                #[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args)]
                fn record_codecs(calls: &mut #calls::Calls) {
                    use #krate::__private::record::{ViaNone as _, ViaWire as _};
                    #((&&#krate::__private::record::Probe::<#call_names>::new()).register(calls);)*
                }
            }

            impl<__S: super::#name> #calls::ServiceDef<__S> for Kind {
                type Router = Router<__S>;
                fn router(svc: ::std::sync::Arc<__S>) -> Router<__S> {
                    Router(svc)
                }

                #engine_def
            }

            /// The tower router (generated): one `Dispatch<Bare>` in, the
            /// call's erased outcome out.
            pub struct Router<__S>(::std::sync::Arc<__S>);

            impl<__S> ::core::clone::Clone for Router<__S> {
                fn clone(&self) -> Self {
                    Router(::std::sync::Arc::clone(&self.0))
                }
            }

            impl<__S: super::#name> #krate::__private::tower::Service<#calls::Dispatch<Bare>> for Router<__S> {
                type Response = #calls::AnyOutcome;
                type Error = #krate::__private::BoxError;
                type Future = #krate::__private::BoxFuture;

                fn poll_ready(
                    &mut self,
                    _: &mut ::core::task::Context<'_>,
                ) -> ::core::task::Poll<::core::result::Result<(), Self::Error>> {
                    ::core::task::Poll::Ready(::core::result::Result::Ok(()))
                }

                fn call(&mut self, d: #calls::Dispatch<Bare>) -> Self::Future {
                    let svc = ::std::sync::Arc::clone(&self.0);
                    ::std::boxed::Box::pin(async move {
                        let #calls::Dispatch { cx, msg } = d;
                        let erased: #calls::AnyOutcome = match msg {
                            #(#dispatch_arms)*
                        };
                        ::core::result::Result::Ok(erased)
                    })
                }
            }
        }

        #[doc = #client_doc]
        #[derive(Clone)]
        #vis struct #client {
            route: #calls::Route<#module::Msg>,
        }

        impl #client {
            #(#client_fns)*
        }

        impl #calls::ServiceClient for #client {
            type Kind = #module::Kind;
            fn from_route(route: #calls::Route<#module::Msg>) -> Self {
                Self { route }
            }
        }

        #[doc = #handle_doc]
        #[derive(Clone)]
        #vis struct #handle {
            route: #calls::Route<#module::Msg>,
            on: #calls::OnBehalf,
        }

        impl #handle {
            /// Calls made from inside a service call act for the same
            /// verified player and join its trace (`cx.player()`,
            /// `cx.trace_id()` propagate).
            pub fn within(&self, cx: &#calls::CallCx) -> Self {
                Self { route: self.route.clone(), on: #calls::OnBehalf::within(cx) }
            }

            /// Calls act for this verified player (admin, boot code).
            pub fn on_behalf_of(&self, who: &#calls::Identity) -> Self {
                Self { route: self.route.clone(), on: #calls::OnBehalf::player(who) }
            }

            #(#handle_fns)*
        }

        impl #calls::ServiceClient for #handle {
            type Kind = #module::Kind;
            fn from_route(route: #calls::Route<#module::Msg>) -> Self {
                Self { route, on: ::core::default::Default::default() }
            }
        }
    })
}

/// Whether `f` carries `#[gsb(engine_events)]` (any other `#[gsb(..)]`
/// is refused).
fn is_engine_events(f: &syn::TraitItemFn) -> syn::Result<bool> {
    let mut found = false;
    for a in f.attrs.iter().filter(|a| a.path().is_ident("gsb")) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("engine_events") && m.input.is_empty() {
                found = true;
                Ok(())
            } else {
                Err(m.error("the only `#[gsb(..)]` on a service method is `#[gsb(engine_events)]`"))
            }
        })?;
    }
    Ok(found)
}

/// The engine-event method: `async fn name(&self, e: EngineEvent)` (or
/// `EngineNotice`), no default body, no generics, returns nothing.
fn engine_method(f: &syn::TraitItemFn) -> syn::Result<EngineMethod> {
    let sig = &f.sig;
    let shape = |span: Span| {
        syn::Error::new(
            span,
            "the engine-event method is `async fn name(&self, e: EngineEvent)` \
             (or `e: EngineNotice`), returning nothing",
        )
    };
    if sig.asyncness.is_none() {
        return Err(shape(sig.fn_token.span()));
    }
    if f.default.is_some() {
        return Err(syn::Error::new(
            sig.ident.span(),
            "service methods have no default body",
        ));
    }
    if !sig.generics.params.is_empty()
        || sig.generics.where_clause.is_some()
        || sig.constness.is_some()
        || sig.unsafety.is_some()
        || sig.abi.is_some()
        || sig.variadic.is_some()
    {
        return Err(shape(sig.span()));
    }
    let name = sig.ident.clone();
    if RESERVED_METHODS.contains(&name.to_string().as_str()) {
        return Err(syn::Error::new(
            name.span(),
            format!("`{name}` is a name the generated service uses; rename the method"),
        ));
    }
    match &sig.output {
        ReturnType::Default => {}
        ReturnType::Type(_, t) if matches!(&**t, Type::Tuple(u) if u.elems.is_empty()) => {}
        ReturnType::Type(_, t) => return Err(shape(t.span())),
    }
    let mut inputs = sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
        _ => return Err(shape(sig.inputs.span())),
    }
    let (arg, ty) = match (inputs.next(), inputs.next()) {
        (Some(FnArg::Typed(p)), None) => {
            let Pat::Ident(id) = &*p.pat else {
                return Err(shape(p.pat.span()));
            };
            if id.by_ref.is_some() || id.subpat.is_some() {
                return Err(shape(p.pat.span()));
            }
            let named = match &*p.ty {
                Type::Path(t) => t
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "EngineEvent" || s.ident == "EngineNotice"),
                _ => false,
            };
            if !named {
                return Err(shape(p.ty.span()));
            }
            (id.ident.clone(), (*p.ty).clone())
        }
        _ => return Err(shape(sig.inputs.span())),
    };
    let attrs = f
        .attrs
        .iter()
        .filter(|a| !a.path().is_ident("gsb"))
        .cloned()
        .collect();
    Ok(EngineMethod {
        attrs,
        name,
        arg,
        ty,
    })
}

fn method(f: &syn::TraitItemFn) -> syn::Result<Method> {
    let sig = &f.sig;
    if sig.asyncness.is_none() {
        return Err(syn::Error::new(
            sig.fn_token.span(),
            "service methods are `async fn`",
        ));
    }
    if f.default.is_some() {
        return Err(syn::Error::new(
            sig.ident.span(),
            "service methods have no default body",
        ));
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "service methods have no generics",
        ));
    }
    if sig.constness.is_some() || sig.unsafety.is_some() || sig.abi.is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "service methods are plain `async fn`s",
        ));
    }
    if sig.variadic.is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "service methods are not variadic",
        ));
    }
    let name = sig.ident.clone();
    if RESERVED_METHODS.contains(&name.to_string().as_str()) {
        return Err(syn::Error::new(
            name.span(),
            format!("`{name}` is a name the generated service uses; rename the method"),
        ));
    }
    let call = format_ident!("{}", camel(&name.to_string()));
    if RESERVED_CALLS.contains(&call.to_string().as_str()) {
        return Err(syn::Error::new(
            name.span(),
            format!(
                "`{name}`'s call type `{call}` collides with a generated item; rename the method"
            ),
        ));
    }
    let mut inputs = sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
        _ => {
            return Err(syn::Error::new(
                sig.inputs.span(),
                "the first parameter is `&self`",
            ));
        }
    }
    let cx = match inputs.next() {
        Some(FnArg::Typed(p)) if matches!(&*p.ty, Type::Reference(r) if r.mutability.is_none()) => {
            p.clone()
        }
        _ => {
            return Err(syn::Error::new(
                sig.inputs.span(),
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
        if id.by_ref.is_some() || id.subpat.is_some() {
            return Err(syn::Error::new(
                p.pat.span(),
                "service arguments are plain names",
            ));
        }
        match &*p.ty {
            Type::Reference(_) => {
                return Err(syn::Error::new(
                    p.ty.span(),
                    "service arguments are owned (a call travels to another task)",
                ));
            }
            Type::ImplTrait(_) => {
                return Err(syn::Error::new(
                    p.ty.span(),
                    "service arguments are concrete types",
                ));
            }
            _ => {}
        }
        args.push((id.ident.clone(), (*p.ty).clone()));
    }
    let (ok, err) = result_types(&sig.output)?;
    let docs = f
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .cloned()
        .collect();
    Ok(Method {
        docs,
        attrs: f.attrs.clone(),
        name,
        call,
        cx,
        args,
        ok,
        err,
    })
}

fn result_types(out: &ReturnType) -> syn::Result<(Type, Type)> {
    let err =
        |span: Span| syn::Error::new(span, "service methods return `Result<Response, Error>`");
    let ReturnType::Type(_, ty) = out else {
        return Err(err(Span::call_site()));
    };
    let Type::Path(p) = &**ty else {
        return Err(err(ty.span()));
    };
    let last = p.path.segments.last().ok_or_else(|| err(ty.span()))?;
    if last.ident != "Result" {
        return Err(err(ty.span()));
    }
    let PathArguments::AngleBracketed(a) = &last.arguments else {
        return Err(err(ty.span()));
    };
    let mut tys = a.args.iter().filter_map(|g| match g {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    match (tys.next(), tys.next(), tys.next()) {
        (Some(ok), Some(e), None) => Ok((ok, e)),
        _ => Err(err(ty.span())),
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

#[cfg(test)]
mod tests {
    use super::{camel, snake};

    #[test]
    fn names() {
        assert_eq!(snake("Inventory"), "inventory");
        assert_eq!(snake("GiftShop"), "gift_shop");
        assert_eq!(camel("load"), "Load");
        assert_eq!(camel("load_all"), "LoadAll");
    }
}
