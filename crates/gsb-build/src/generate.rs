//! The code: prost-build's [`ServiceGenerator`] hook writes, inside the
//! request's own package module, one `impl Call` + `impl Request` per
//! declared rpc.

use std::collections::HashMap;

use heck::{ToSnakeCase, ToUpperCamelCase};
use prost_build::{Service, ServiceGenerator};

use crate::check::{Index, Plan};

/// `(package, service, method)` → (op, the error message's full name).
type Table = HashMap<(String, String, String), (u16, String)>;

pub(crate) struct Requests {
    table: Table,
    index: Index,
    externs: Vec<(String, String)>,
    contract: String,
}

impl Requests {
    pub(crate) fn new(
        plan: &Plan,
        index: Index,
        externs: Vec<(String, String)>,
        contract: String,
    ) -> Self {
        let table = plan
            .requests
            .iter()
            .map(|d| {
                (
                    (d.package.clone(), d.service.clone(), d.method.clone()),
                    (d.op, d.error.clone()),
                )
            })
            .collect();
        Self {
            table,
            index,
            externs,
            contract,
        }
    }

    /// The Rust path of message `full` as seen from package `from`'s
    /// module — what prost-build itself would write: an `extern_path`
    /// prefix if one matches, else `super::` up to the common package and
    /// down again.
    pub(crate) fn rust_path(&self, from: &str, full: &str) -> String {
        if let Some((proto, rust)) = self
            .externs
            .iter()
            .filter(|(p, _)| full == p || full.starts_with(&format!("{p}.")))
            .max_by_key(|(p, _)| p.len())
        {
            let rest: Vec<&str> = full[proto.len()..]
                .split('.')
                .filter(|s| !s.is_empty())
                .collect();
            return join(rust.clone(), &nested(&rest));
        }
        let Some(place) = self.index.get(full) else {
            // Unreachable after the checks (the name resolved); keep the
            // build's error readable rather than panicking here.
            return full.trim_start_matches('.').replace('.', "::");
        };
        let here = segments(from);
        let there = segments(&place.package);
        let common = here.iter().zip(&there).take_while(|(a, b)| a == b).count();
        let mut parts: Vec<String> = vec!["super".to_owned(); here.len() - common];
        parts.extend(there[common..].iter().cloned());
        let names: Vec<&str> = place.nesting.iter().map(String::as_str).collect();
        parts.extend(nested(&names));
        parts.join("::")
    }
}

fn join(base: String, rest: &[String]) -> String {
    if rest.is_empty() {
        base
    } else {
        format!("{base}::{}", rest.join("::"))
    }
}

/// A package's module path, as prost names it.
fn segments(package: &str) -> Vec<String> {
    if package.is_empty() {
        Vec::new()
    } else {
        package.split('.').map(to_snake).collect()
    }
}

/// `["Outer", "Inner"]` → `["outer", "Inner"]` (prost puts a nested
/// message in its parent's snake-case module).
fn nested(names: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if i + 1 == names.len() {
            out.push(to_upper_camel(n));
        } else {
            out.push(to_snake(n));
        }
    }
    out
}

/// prost-build's identifier rules (its `ident.rs`): heck's case change,
/// then keywords made legal.
fn sanitize(ident: String) -> String {
    match ident.as_str() {
        "as" | "break" | "const" | "continue" | "else" | "enum" | "false" | "fn" | "for" | "if"
        | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref"
        | "return" | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use"
        | "where" | "while" | "dyn" | "abstract" | "become" | "box" | "do" | "final" | "macro"
        | "override" | "priv" | "typeof" | "unsized" | "virtual" | "yield" | "async" | "await"
        | "try" | "gen" => format!("r#{ident}"),
        "_" | "super" | "self" | "Self" | "extern" | "crate" => format!("{ident}_"),
        s if s.starts_with(|c: char| c.is_numeric()) => format!("_{ident}"),
        _ => ident,
    }
}

fn to_snake(s: &str) -> String {
    sanitize(s.to_snake_case())
}

fn to_upper_camel(s: &str) -> String {
    sanitize(s.to_upper_camel_case())
}

impl ServiceGenerator for Requests {
    fn generate(&mut self, service: Service, buf: &mut String) {
        for m in &service.methods {
            let key = (
                service.package.clone(),
                service.proto_name.clone(),
                m.proto_name.clone(),
            );
            let Some((op, error)) = self.table.get(&key) else {
                continue; // not a gsb request (an ordinary service)
            };
            let error = self.rust_path(&service.package, error);
            let c = &self.contract;
            let (req, resp) = (&m.input_type, &m.output_type);
            let rpc = if service.package.is_empty() {
                format!("{}.{}", service.proto_name, m.proto_name)
            } else {
                format!(
                    "{}.{}.{}",
                    service.package, service.proto_name, m.proto_name
                )
            };
            buf.push_str(&format!(
                "/// gsb request `{rpc}` (op {op}): answered with `{resp}`, or the typed `{error}`.\n\
                 impl {c}::Call for {req} {{\n    \
                     type Response = {resp};\n    \
                     type Error = {error};\n\
                 }}\n\
                 impl {c}::Request for {req} {{\n    \
                     const OP: u16 = {op};\n\
                 }}\n"
            ));
        }
    }
}
