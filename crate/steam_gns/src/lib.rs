//! GNS P2P media transport for Steam voice — **scaffold**.
//!
//! Wraps `gns-rs` (Valve's open-source GameNetworkingSockets) to bring up the
//! direct-P2P audio path: custom signaling (rendezvous relayed over the Steam
//! CM) plus our phase-1 networking cert, bridged to the messenger audio graph.
//! See `interface/steam/VOICE_PROTOCOL.md` ("Media build plan").
//!
//! Right now this crate exists only to verify the `gns-sys` C++ build wires up
//! in the Nix dev shell; the transport itself is not implemented yet.

/// Placeholder until the transport lands; keeps the crate non-empty.
pub fn placeholder() {}
