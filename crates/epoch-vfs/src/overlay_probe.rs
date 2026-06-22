//! The monolithic `OverlayProbeVfs` has been decomposed into a decorator stack.
//!
//! Its three former concerns now live in dedicated modules:
//!
//! - lower-dir read-through → [`crate::passthrough::PassthroughVfs`]
//! - synthetic `.vfs` control plane → [`crate::control_plane::ControlPlaneVfs`]
//! - per-call logging → [`crate::logging::LoggingVfs`]
//!
//! Use the [`crate::overlay_probe`] factory to compose the equivalent stack.
//!
//! This module is intentionally empty and is no longer declared in `lib.rs`; it
//! remains only as a breadcrumb until it can be deleted from version control.
