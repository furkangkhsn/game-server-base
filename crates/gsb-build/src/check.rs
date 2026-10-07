//! The build-time checks (`docs/SERVICES.md` §11): read every `service`
//! of the descriptor set, keep the gsb requests, refuse what must never
//! ship. Pure — bytes in, a plan or the full list of problems out.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use prost::Message as _;

use crate::descriptor::{FileSet, Message};

/// The first op a game may use: the inner ops below it are the engine's
/// (the base band's numbering, `gsb_protocol::op::GAME_BAND_START`;
/// pinned against it by this crate's tests).
pub const GAME_BAND_START: u16 = 1000;

/// One request the build declares: an `rpc` with `(gsb.op)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    /// The protobuf package (`""` for none).
    pub package: String,
    /// The service's name as written.
    pub service: String,
    /// The rpc's name as written.
    pub method: String,
    /// The request message, fully qualified (`.pkg.TransferGold`).
    pub request: String,
    /// The response message, fully qualified.
    pub response: String,
    /// The typed error message, fully qualified (resolved from
    /// `(gsb.error)`).
    pub error: String,
    /// The inner op.
    pub op: u16,
}

impl Declared {
    /// `pkg.Service.Method` — how problems name an rpc.
    pub fn rpc(&self) -> String {
        rpc_name(&self.package, &self.service, &self.method)
    }
}

/// What a descriptor set declares: its requests (in declaration order)
/// and the ops its services reserve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Every request, in file / service / rpc order.
    pub requests: Vec<Declared>,
    /// Every `(gsb.reserved_op)` of every service.
    pub reserved: BTreeSet<u16>,
}

/// One reason a build is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The descriptor set itself did not decode.
    Descriptor(String),
    /// An rpc of a gsb service (one where some rpc carries `(gsb.op)` or
    /// `(gsb.error)`) without `(gsb.op)`.
    MissingOp {
        /// `pkg.Service.Method`.
        rpc: String,
    },
    /// An rpc with `(gsb.op)` but no (or an empty) `(gsb.error)`.
    MissingError {
        /// `pkg.Service.Method`.
        rpc: String,
    },
    /// `(gsb.error)` names no message of the build.
    UnknownError {
        /// `pkg.Service.Method`.
        rpc: String,
        /// The name as written.
        error: String,
    },
    /// An op below the game band — the engine's range.
    EngineRange {
        /// `pkg.Service.Method`.
        rpc: String,
        /// The op as written.
        op: u32,
    },
    /// An op that does not fit the wire's 16 bits.
    TooLarge {
        /// `pkg.Service.Method`.
        rpc: String,
        /// The op as written.
        op: u32,
    },
    /// Two rpcs, one op.
    DuplicateOp {
        /// The op.
        op: u16,
        /// The rpc that took it first.
        first: String,
        /// The rpc that took it again.
        second: String,
    },
    /// An rpc takes an op a service lists as `(gsb.reserved_op)`.
    ReservedOp {
        /// The op.
        op: u16,
        /// `pkg.Service.Method`.
        rpc: String,
    },
    /// One request message behind two rpcs (it can have one `OP`).
    SharedRequest {
        /// The message, fully qualified.
        request: String,
        /// The rpc that used it first.
        first: String,
        /// The rpc that used it again.
        second: String,
    },
    /// A streaming rpc: a gsb request is one message each way.
    Streaming {
        /// `pkg.Service.Method`.
        rpc: String,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Descriptor(e) => write!(f, "the descriptor set did not decode: {e}"),
            Problem::MissingOp { rpc } => write!(
                f,
                "rpc {rpc} has no `option (gsb.op)` (every rpc of a gsb service needs one)"
            ),
            Problem::MissingError { rpc } => {
                write!(
                    f,
                    "rpc {rpc} has no `option (gsb.error)` naming its error message"
                )
            }
            Problem::UnknownError { rpc, error } => {
                write!(
                    f,
                    "rpc {rpc}: `(gsb.error)` = {error:?} names no message of this build"
                )
            }
            Problem::EngineRange { rpc, op } => write!(
                f,
                "rpc {rpc}: op {op} is in the engine's range (game ops start at {GAME_BAND_START})"
            ),
            Problem::TooLarge { rpc, op } => {
                write!(f, "rpc {rpc}: op {op} does not fit 16 bits")
            }
            Problem::DuplicateOp { op, first, second } => {
                write!(f, "op {op} is taken twice: by {first} and by {second}")
            }
            Problem::ReservedOp { op, rpc } => write!(
                f,
                "rpc {rpc}: op {op} is reserved (`(gsb.reserved_op)`: retired, never reused)"
            ),
            Problem::SharedRequest {
                request,
                first,
                second,
            } => write!(
                f,
                "message {request} is the request of two rpcs ({first}, {second}); \
                 a request type has one op — give each rpc its own message"
            ),
            Problem::Streaming { rpc } => {
                write!(
                    f,
                    "rpc {rpc} streams; a gsb request is one message each way"
                )
            }
        }
    }
}

fn rpc_name(package: &str, service: &str, method: &str) -> String {
    if package.is_empty() {
        format!("{service}.{method}")
    } else {
        format!("{package}.{service}.{method}")
    }
}

/// Where a message lives: its package and its nesting path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub package: String,
    pub nesting: Vec<String>,
}

/// Every message of the set by its fully qualified name.
pub(crate) type Index = HashMap<String, Place>;

fn index_messages(set: &FileSet) -> Index {
    fn walk(idx: &mut Index, package: &str, prefix: &[String], m: &Message) {
        let mut nesting = prefix.to_vec();
        nesting.push(m.name.clone().unwrap_or_default());
        let full = if package.is_empty() {
            format!(".{}", nesting.join("."))
        } else {
            format!(".{package}.{}", nesting.join("."))
        };
        idx.insert(
            full,
            Place {
                package: package.to_owned(),
                nesting: nesting.clone(),
            },
        );
        for n in &m.nested_type {
            walk(idx, package, &nesting, n);
        }
    }
    let mut idx = Index::new();
    for file in &set.file {
        let package = file.package.clone().unwrap_or_default();
        for m in &file.message_type {
            walk(&mut idx, &package, &[], m);
        }
    }
    idx
}

/// Resolve `(gsb.error)` the way protobuf resolves a type name: a
/// leading `.` is fully qualified; otherwise the innermost enclosing
/// scope (the package, then its parents, then the root) that has it.
fn resolve(idx: &Index, package: &str, name: &str) -> Option<String> {
    if let Some(full) = name.strip_prefix('.') {
        let full = format!(".{full}");
        return idx.contains_key(&full).then_some(full);
    }
    let mut scope: Vec<&str> = if package.is_empty() {
        Vec::new()
    } else {
        package.split('.').collect()
    };
    loop {
        let full = if scope.is_empty() {
            format!(".{name}")
        } else {
            format!(".{}.{name}", scope.join("."))
        };
        if idx.contains_key(&full) {
            return Some(full);
        }
        scope.pop()?;
    }
}

/// Decode, index and check a descriptor set (protoc's `-o` output, with
/// imports). `Ok` only when nothing is wrong; `Err` lists EVERY problem.
pub fn check(fds: &[u8]) -> Result<Plan, Vec<Problem>> {
    let (set, idx) = decode(fds)?;
    plan(&set, &idx)
}

pub(crate) fn plan(set: &FileSet, idx: &Index) -> Result<Plan, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut plan = Plan::default();
    for file in &set.file {
        for svc in &file.service {
            if let Some(o) = &svc.options {
                for &op in &o.reserved_op {
                    if let Ok(op) = u16::try_from(op) {
                        plan.reserved.insert(op);
                    }
                }
            }
        }
    }
    let mut by_op: BTreeMap<u16, String> = BTreeMap::new();
    let mut by_request: HashMap<String, String> = HashMap::new();
    for file in &set.file {
        let package = file.package.clone().unwrap_or_default();
        for svc in &file.service {
            let service = svc.name.clone().unwrap_or_default();
            let is_gsb = svc.method.iter().any(|m| {
                m.options
                    .as_ref()
                    .is_some_and(|o| o.op.is_some() || o.error.is_some())
            });
            if !is_gsb {
                continue; // an ordinary protobuf service: not ours
            }
            for m in &svc.method {
                let method = m.name.clone().unwrap_or_default();
                let rpc = rpc_name(&package, &service, &method);
                let opts = m.options.clone().unwrap_or_default();
                if m.client_streaming.unwrap_or(false) || m.server_streaming.unwrap_or(false) {
                    problems.push(Problem::Streaming { rpc: rpc.clone() });
                }
                let error = match opts.error.as_deref() {
                    None | Some("") => {
                        problems.push(Problem::MissingError { rpc: rpc.clone() });
                        None
                    }
                    Some(e) => {
                        let r = resolve(idx, &package, e);
                        if r.is_none() {
                            problems.push(Problem::UnknownError {
                                rpc: rpc.clone(),
                                error: e.to_owned(),
                            });
                        }
                        r
                    }
                };
                let op = match opts.op {
                    None => {
                        problems.push(Problem::MissingOp { rpc: rpc.clone() });
                        None
                    }
                    Some(op) if op < u32::from(GAME_BAND_START) => {
                        problems.push(Problem::EngineRange {
                            rpc: rpc.clone(),
                            op,
                        });
                        None
                    }
                    Some(op) => match u16::try_from(op) {
                        Ok(op) => Some(op),
                        Err(_) => {
                            problems.push(Problem::TooLarge {
                                rpc: rpc.clone(),
                                op,
                            });
                            None
                        }
                    },
                };
                if let Some(op) = op {
                    if plan.reserved.contains(&op) {
                        problems.push(Problem::ReservedOp {
                            op,
                            rpc: rpc.clone(),
                        });
                    }
                    if let Some(first) = by_op.get(&op) {
                        problems.push(Problem::DuplicateOp {
                            op,
                            first: first.clone(),
                            second: rpc.clone(),
                        });
                    } else {
                        by_op.insert(op, rpc.clone());
                    }
                }
                let request = m.input_type.clone().unwrap_or_default();
                if let Some(first) = by_request.get(&request) {
                    problems.push(Problem::SharedRequest {
                        request: request.clone(),
                        first: first.clone(),
                        second: rpc.clone(),
                    });
                } else {
                    by_request.insert(request.clone(), rpc.clone());
                }
                if let (Some(op), Some(error)) = (op, error) {
                    plan.requests.push(Declared {
                        package: package.clone(),
                        service: service.clone(),
                        method,
                        request,
                        response: m.output_type.clone().unwrap_or_default(),
                        error,
                        op,
                    });
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(plan)
    } else {
        Err(problems)
    }
}

pub(crate) fn decode(fds: &[u8]) -> Result<(FileSet, Index), Vec<Problem>> {
    let set = FileSet::decode(fds).map_err(|e| vec![Problem::Descriptor(e.to_string())])?;
    let idx = index_messages(&set);
    Ok((set, idx))
}
