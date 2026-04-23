//! Driven adapters. Concrete implementations of the ports defined in
//! `super::ports`. Per AD-18, co-located with the component.

pub mod pty_local;
pub mod pty_ssh;
pub mod sqlite_store;
