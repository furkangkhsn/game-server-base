//! The proposed `gsb-build` extension (docs/EVENTS.md §4.1), small enough
//! to prove the option form: read protoc's descriptor set — with the new
//! `(gsb.event)` message option — check it, and write `impl Event` +
//! the destination marker per event message.
//!
//! The checks run over ONE op space shared by requests and events (the
//! game band; a dispatcher, a record and a log name an op without saying
//! which envelope carried it): a duplicate op across the two is refused,
//! as is an op below the game band, above 16 bits, reserved
//! (`(gsb.reserved_op)` on a service, or the new file-level
//! `(gsb.reserved_ops)`), an event without `to:`, and a message that is
//! both an rpc's request and an event.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use prost::Message as _;

use crate::contract::Dest;

/// The first op a game may use.
pub const GAME_BAND_START: u16 = 1000;

/// Trimmed `descriptor.proto` mirrors that KEEP gsb's options (prost's
/// own types drop extensions — gsb-build's finding, q3).
mod descriptor {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FileSet {
        #[prost(message, repeated, tag = "1")]
        pub file: Vec<File>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct File {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(string, optional, tag = "2")]
        pub package: Option<String>,
        #[prost(message, repeated, tag = "4")]
        pub message_type: Vec<Message>,
        #[prost(message, repeated, tag = "6")]
        pub service: Vec<Service>,
        #[prost(message, optional, tag = "8")]
        pub options: Option<FileOptions>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FileOptions {
        /// NEW `(gsb.reserved_ops)` (50005).
        #[prost(uint32, repeated, packed = "false", tag = "50005")]
        pub reserved_ops: Vec<u32>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Message {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(message, repeated, tag = "3")]
        pub nested_type: Vec<Message>,
        #[prost(message, optional, tag = "7")]
        pub options: Option<MessageOptions>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct MessageOptions {
        /// NEW `(gsb.event)` (50004).
        #[prost(message, optional, tag = "50004")]
        pub event: Option<EventOptions>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct EventOptions {
        #[prost(uint32, optional, tag = "1")]
        pub op: Option<u32>,
        #[prost(int32, optional, tag = "2")]
        pub to: Option<i32>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Service {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(message, repeated, tag = "2")]
        pub method: Vec<Method>,
        #[prost(message, optional, tag = "3")]
        pub options: Option<ServiceOptions>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Method {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(string, optional, tag = "2")]
        pub input_type: Option<String>,
        #[prost(message, optional, tag = "4")]
        pub options: Option<MethodOptions>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct MethodOptions {
        #[prost(uint32, optional, tag = "50001")]
        pub op: Option<u32>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ServiceOptions {
        #[prost(uint32, repeated, packed = "false", tag = "50003")]
        pub reserved_op: Vec<u32>,
    }
}

/// One declared event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredEvent {
    /// Fully qualified (`.pkg.GoldReceived`).
    pub message: String,
    /// The Rust type name (nested: `Outer::Inner` is not handled here).
    pub rust: String,
    /// The op.
    pub op: u16,
    /// The destination.
    pub to: Dest,
}

/// What a build declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Events, in file / message order.
    pub events: Vec<DeclaredEvent>,
    /// Request ops (`(gsb.op)` per rpc): op → request message.
    pub requests: BTreeMap<u16, String>,
    /// Reserved ops (both spaces).
    pub reserved: BTreeSet<u16>,
}

/// One reason a build is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The set did not decode.
    Descriptor(String),
    /// `(gsb.event)` without `op`.
    NoOp(String),
    /// `(gsb.event)` without `to` (or `DESTINATION_UNSPECIFIED`).
    NoDestination(String),
    /// Below the game band (the engine's range).
    EngineOp(String, u32),
    /// Above 16 bits.
    OpTooLarge(String, u32),
    /// Two declarations (events and/or requests) on one op.
    Duplicate { op: u16, first: String, second: String },
    /// An op the build reserved.
    Reserved(String, u16),
    /// A message that is an rpc's request AND an event.
    EventIsRequest(String),
}

/// Read and check a descriptor set (`protoc --include_imports
/// --descriptor_set_out`).
pub fn plan(set: &[u8]) -> Result<Plan, Vec<Problem>> {
    let set = descriptor::FileSet::decode(set).map_err(|e| vec![Problem::Descriptor(e.to_string())])?;
    let mut plan = Plan::default();
    let mut problems = Vec::new();
    let mut owners: BTreeMap<u16, String> = BTreeMap::new();
    let mut request_types = BTreeSet::new();
    for f in &set.file {
        if let Some(o) = &f.options {
            plan.reserved.extend(o.reserved_ops.iter().map(|&x| x as u16));
        }
        let pkg = f.package.clone().unwrap_or_default();
        for s in &f.service {
            if let Some(o) = &s.options {
                plan.reserved.extend(o.reserved_op.iter().map(|&x| x as u16));
            }
            for m in &s.method {
                let Some(op) = m.options.as_ref().and_then(|o| o.op) else { continue };
                let name = m.input_type.clone().unwrap_or_default();
                request_types.insert(name.clone());
                match check_op(&name, op) {
                    Ok(op) => {
                        claim(&mut owners, &mut problems, op, &name);
                        plan.requests.insert(op, name);
                    }
                    Err(p) => problems.push(p),
                }
            }
        }
        for m in &f.message_type {
            let Some(ev) = m.options.as_ref().and_then(|o| o.event.as_ref()) else { continue };
            let short = m.name.clone().unwrap_or_default();
            let full = if pkg.is_empty() { format!(".{short}") } else { format!(".{pkg}.{short}") };
            let to = match ev.to {
                Some(1) => Some(Dest::World),
                Some(2) => Some(Dest::Client),
                Some(3) => Some(Dest::Both),
                _ => None,
            };
            let Some(to) = to else {
                problems.push(Problem::NoDestination(full));
                continue;
            };
            let Some(op) = ev.op else {
                problems.push(Problem::NoOp(full));
                continue;
            };
            match check_op(&full, op) {
                Ok(op) => {
                    claim(&mut owners, &mut problems, op, &full);
                    plan.events.push(DeclaredEvent {
                        message: full,
                        rust: short,
                        op,
                        to,
                    });
                }
                Err(p) => problems.push(p),
            }
        }
    }
    for e in &plan.events {
        if request_types.contains(&e.message) {
            problems.push(Problem::EventIsRequest(e.message.clone()));
        }
    }
    for (op, name) in &owners {
        if plan.reserved.contains(op) {
            problems.push(Problem::Reserved(name.clone(), *op));
        }
    }
    if problems.is_empty() { Ok(plan) } else { Err(problems) }
}

fn check_op(name: &str, op: u32) -> Result<u16, Problem> {
    if op > u32::from(u16::MAX) {
        return Err(Problem::OpTooLarge(name.to_owned(), op));
    }
    if op < u32::from(GAME_BAND_START) {
        return Err(Problem::EngineOp(name.to_owned(), op));
    }
    Ok(op as u16)
}

fn claim(owners: &mut BTreeMap<u16, String>, problems: &mut Vec<Problem>, op: u16, name: &str) {
    if let Some(first) = owners.get(&op) {
        problems.push(Problem::Duplicate {
            op,
            first: first.clone(),
            second: name.to_owned(),
        });
    } else {
        owners.insert(op, name.to_owned());
    }
}

/// The generated text (the exact lines `src/game.rs` carries).
pub fn generate(plan: &Plan) -> String {
    let mut out = String::new();
    for e in &plan.events {
        let to = match e.to {
            Dest::World => "World",
            Dest::Client => "Client",
            Dest::Both => "Both",
        };
        let _ = writeln!(out, "impl Event for {} {{ const OP: u16 = {}; const TO: Dest = Dest::{to}; }}", e.rust, e.op);
        if e.to.world() {
            let _ = writeln!(out, "impl WorldEvent for {} {{}}", e.rust);
        }
        if e.to.client() {
            let _ = writeln!(out, "impl ClientEvent for {} {{}}", e.rust);
        }
    }
    out
}
