//! The orchestrator: spawn pinned client processes, read their
//! RESULT lines back, and fold them into one report.

use super::*;
use crate::client::*;
use crate::codec::*;
use crate::report::*;
use crate::serve::*;
use crate::server::*;

mod pinning;
pub(crate) use pinning::*;
