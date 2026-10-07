//! Build-script helper for gsb games (`docs/SERVICES.md` §4.2, §11, Faz
//! 3): prost-build, plus the request codegen of `gsb/options.proto`.
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

pub use check::{Declared, GAME_BAND_START, Plan, Problem, check};

/// The default path of the request contract in generated code.
pub const CONTRACT_PATH: &str = "::gsb_protocol::request";

/// Why a build was refused.
#[derive(Debug)]
pub enum Error {
    /// protoc or prost-build failed (I/O, a `.proto` syntax error …).
    Build(std::io::Error),
    /// The requests broke a rule; every problem found.
    Invalid(Vec<Problem>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Build(e) => write!(f, "gsb-build: {e}"),
            Error::Invalid(ps) => {
                writeln!(f, "gsb-build: the .proto requests are invalid:")?;
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

    /// Write the generated files here instead of `OUT_DIR`.
    pub fn out_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        let dir = dir.into();
        self.config.out_dir(&dir);
        self.out_dir = Some(dir);
        self
    }

    /// Compile `protos` (with `includes`, then the builder's own) into
    /// prost messages plus the request impls; check the requests first.
    /// Returns what was declared.
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
        self.config
            .service_generator(Box::new(generate::Requests::new(
                &plan,
                index,
                self.externs.clone(),
                self.contract.clone(),
            )));
        self.config.compile_fds(fds)?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests;
