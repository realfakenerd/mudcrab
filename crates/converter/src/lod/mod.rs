//! Offline LOD chunk compiler: `references` + `statics` + cell cache into
//! spatial GLB chunks with a database index (ADR-0010, `lod-compiler.md`).
//!
//! Phase 1 compiles terrain only. Phase 2 adds eligible static objects into
//! the same chunk files beneath per-cell `objects` groups.

pub mod albedo;
pub mod terrain;
