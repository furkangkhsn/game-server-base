//! Build-script helper for gsb games (`docs/SERVICES.md` §4.2, §11, Faz
//! 3; `docs/EVENTS.md` §4.1): prost-build, plus the request and event
//! codegen of `gsb/options.proto`.
//!
//! A game declares its client-wire requests as `rpc`s in its `.proto`:
//!
//! ```proto
//! import "gsb/options.proto";
//!
//! service Game {
//!   option (gsb.reserved_op) = 1002;          // retired: never reused
//!   rpc TransferGold(TransferGold) returns (TransferDone) {
//!     option (gsb.op) = 1001;
//!     option (gsb.error) = "GameError";
//!   }
//! }
//! ```
//!
//! and its `build.rs` compiles with [`Builder`] instead of a bare
//! `prost_build::Config`:
//!
//! ```rust,no_run
//! let mut b = gsb_build::Builder::new();
//! b.extern_path(".gsb.base", "::gsb_protocol::base");
//! b.compile(&["proto/game.proto"], &["proto"]).unwrap_or_else(|e| panic!("{e}"));
//! ```
//!
//! Next to prost's messages, the package module then holds, per rpc:
//!
//! ```rust,ignore
//! impl ::gsb_protocol::request::Call for TransferGold { type Response = TransferDone; type Error = GameError; }
//! impl ::gsb_protocol::request::Request for TransferGold { const OP: u16 = 1001; }
//! ```
//!
//! — the contract the server's call table (`gsb_core::calls`, which
//! re-exports these traits) and the typed client
//! (`gsb_client::requests`) both speak. The build is REFUSED (every
//! problem listed — [`Problem`]) for a duplicate op, an op in the engine's
//! range (below [`GAME_BAND_START`]), an op a service reserves, an rpc
//! without `(gsb.error)` or naming no message, a streaming rpc, or one
//! request message behind two rpcs. A service none of whose rpcs carries
//! a gsb option is an ordinary protobuf service and left alone.
//!
//! Events (`docs/EVENTS.md` §4.1) are declared on the message itself:
//!
//! ```proto
//! option (gsb.reserved_ops) = 2099;   // file level: retired, never reused
//!
//! message GoldReceived {
//!   option (gsb.event) = { op: 2001, to: WORLD };   // WORLD | CLIENT | BOTH
//!   uint64 amount = 1;
//! }
//! ```
//!
//! and get, at the end of their file's code:
//!
//! ```rust,ignore
//! impl ::gsb_protocol::event::Event for GoldReceived { const OP: u16 = 2001; const TO: Dest = Dest::World; }
//! impl ::gsb_protocol::event::WorldEvent for GoldReceived {}   // ClientEvent for CLIENT; both for BOTH
//! ```
//!
//! Requests and events share ONE op space (the game band): an op both
//! take is a [`Problem::DuplicateOp`], and the reserved ops
//! (`(gsb.reserved_op)` on a service, `(gsb.reserved_ops)` on a file)
//! close a number to both. An event is also refused for a missing `op`
//! or `to`, an op in the engine's range or past 16 bits, and a message
//! that is a gsb rpc's request as well (its type would have two ops).
//!
//! Pure Rust: the options are read from protoc's descriptor set with
//! trimmed `descriptor.proto` mirrors (prost drops extensions), and the
//! code is written through prost-build's own `ServiceGenerator` hook —
//! no protoc plugin binary. protoc is the vendored one every gsb crate
//! builds with (`protoc-bin-vendored`); `gsb/options.proto` is found
//! through gsb-protocol's `links` metadata (`DEP_GSB_BASE_PROTO_DIR`, so
//! the game depends on gsb-protocol directly, as it already must for
//! `base.proto`).

mod check;
mod descriptor;
mod generate;

use std::fmt;
use std::path::{Path, PathBuf};

pub use check::{Declared, DeclaredEvent, Destination, GAME_BAND_START, Plan, Problem, check};

/// The default path of the request contract in generated code.
pub const CONTRACT_PATH: &str = "::gsb_protocol::request";

/// The default path of the event contract in generated code.
pub const EVENT_CONTRACT_PATH: &str = "::gsb_protocol::event";

/// Why a build was refused.
#[derive(Debug)]
pub enum Error {
    /// protoc or prost-build failed (I/O, a `.proto` syntax error …).
    Build(std::io::Error),
    /// The requests or events broke a rule; every problem found.
    Invalid(Vec<Problem>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Build(e) => write!(f, "gsb-build: {e}"),
            Error::Invalid(ps) => {
                writeln!(f, "gsb-build: the .proto requests/events are invalid:")?;
                for p in ps {
                    writeln!(f, "  - {p}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Build(e)
    }
}

/// prost-build with the gsb request codegen (see the crate docs).
pub struct Builder {
    config: prost_build::Config,
    includes: Vec<PathBuf>,
    externs: Vec<(String, String)>,
    contract: String,
    event_contract: String,
    out_dir: Option<PathBuf>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// A builder on the vendored protoc (falling back to prost-build's
    /// `PROTOC`/`PATH` lookup on a target it ships no binary for), with
    /// protoc's own include directory (for `google/protobuf/*.proto`) and
    /// gsb-protocol's proto directory (`DEP_GSB_BASE_PROTO_DIR`, for
    /// `base.proto` and `gsb/options.proto`) appended to every compile's
    /// includes.
    pub fn new() -> Self {
        let mut config = prost_build::Config::new();
        let mut includes = Vec::new();
        if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
            config.protoc_executable(protoc);
            if let Ok(inc) = protoc_bin_vendored::include_path() {
                includes.push(inc);
            }
        }
        if let Some(dir) = std::env::var_os("DEP_GSB_BASE_PROTO_DIR") {
            includes.push(PathBuf::from(dir));
        }
        Self {
            config,
            includes,
            externs: Vec::new(),
            contract: CONTRACT_PATH.to_owned(),
            event_contract: EVENT_CONTRACT_PATH.to_owned(),
            out_dir: None,
        }
    }

    /// The underlying prost-build config, for anything this builder does
    /// not wrap (attributes, `bytes`, `btree_map` …). Do NOT set a
    /// `service_generator` (the builder owns it) or an `extern_path`
    /// here (use [`Self::extern_path`]: the request codegen must see it).
    pub fn config(&mut self) -> &mut prost_build::Config {
        &mut self.config
    }

    /// `prost_build::Config::extern_path`, also used to name an error
    /// message of an extern package.
    pub fn extern_path(&mut self, proto: &str, rust: &str) -> &mut Self {
        self.config.extern_path(proto, rust);
        self.externs.push((proto.to_owned(), rust.to_owned()));
        self
    }

    /// Where the generated impls find `Call` and `Request` (default
    /// [`CONTRACT_PATH`]; `::gsb_core::calls` is the same traits).
    pub fn contract_path(&mut self, path: &str) -> &mut Self {
        path.clone_into(&mut self.contract);
        self
    }

    /// Where the generated impls find `Event`, `WorldEvent`,
    /// `ClientEvent` and `Dest` (default [`EVENT_CONTRACT_PATH`]).
    pub fn event_contract_path(&mut self, path: &str) -> &mut Self {
        path.clone_into(&mut self.event_contract);
        self
    }

    /// Write the generated files here instead of `OUT_DIR`.
    pub fn out_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        let dir = dir.into();
        self.config.out_dir(&dir);
        self.out_dir = Some(dir);
        self
    }

    /// Compile `protos` (with `includes`, then the builder's own) into
    /// prost messages plus the request and event impls; check the
    /// requests and events first. Returns what was declared.
    pub fn compile(
        &mut self,
        protos: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> Result<Plan, Error> {
        let out = match &self.out_dir {
            Some(d) => d.clone(),
            None => PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| {
                std::io::Error::other("OUT_DIR is not set (call from a build script, or out_dir)")
            })?),
        };
        let fds_path = out.join("gsb-build.fds");
        self.config.file_descriptor_set_path(&fds_path);
        let mut all: Vec<PathBuf> = includes.iter().map(|p| p.as_ref().to_owned()).collect();
        all.extend(self.includes.iter().cloned());
        let fds = self.config.load_fds(protos, &all)?;
        let raw = std::fs::read(&fds_path)?;
        let (set, index) = check::decode(&raw).map_err(Error::Invalid)?;
        let plan = check::plan(&set, &index).map_err(Error::Invalid)?;
        let files: Vec<String> = set
            .file
            .iter()
            .map(|f| f.name.clone().unwrap_or_default())
            .collect();
        let (generator, finalized) =
            generate::Requests::new(&plan, index, self.externs.clone(), self.contract.clone())
                .with_events(&plan, &files, &self.event_contract);
        self.config.service_generator(Box::new(generator));
        self.config.compile_fds(fds)?;
        // The event impls ride prost-build's per-file `finalize` (see
        // `generate`): fewer or more calls than files means they landed
        // in the wrong file or not at all.
        if finalized.get() != files.len() {
            return Err(Error::Build(std::io::Error::other(format!(
                "prost-build finalized {} files of {}: the event impls could not be placed",
                finalized.get(),
                files.len()
            ))));
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests;
