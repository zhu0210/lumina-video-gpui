//! wgpu frame-import boundary.
//!
//! This crate will own native-memory import, GPU conversion, and completion
//! lifetime.  wgpu types remain private to this crate's future implementation;
//! the migration seam consumes core session descriptions and owned native
//! frame leases.  This ticket intentionally adds no importer or player stub.
