//! The human-readable report and the single scriptable RESULT line.

use super::*;
use crate::client::*;
use crate::stats::*;

mod fold;
mod result;
mod spread;
mod team;
pub(crate) use fold::*;
pub(crate) use result::*;
pub(crate) use spread::*;
pub(crate) use team::*;

/// Extra facts of a separate-process (orchestrated) run; `None` for the
/// in-process and external-direct modes.
pub(crate) struct SepInfo {
    pub(crate) procs: u32,
    pub(crate) server_pid: u32,
    pub(crate) client_pids: Vec<u32>,
    /// The disjoint core sets (`taskset` masks), for the record: e.g.
    /// `server:0,1,2,3;client0:4,5,6,7;client1:8,9,10,11` — or `none`
    /// when pinning was not possible.
    pub(crate) affinity: String,
    /// CPU seconds the server process used over the run (from
    /// `/proc/<pid>/stat`) — the isolation proof: with disjoint masks,
    /// server CPU cannot hide behind client decode.
    pub(crate) server_cpu_s: f64,
    pub(crate) clients_cpu_s: f64,
}
