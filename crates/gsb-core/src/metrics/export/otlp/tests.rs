//! Unit tests of the OTLP exporter's private halves: the hand-off
//! (cadence, backpressure), the endpoint grammar, and the wire tags.

use super::*;

mod endpoint;
mod handoff;
mod wire;
