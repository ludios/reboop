// Model-output: Claude Opus 5.5

//! Primitives for carefully rebooting NixOS machines: see doc/spec.md.

pub mod btrfs;
pub mod child;
pub mod config;
pub mod deadline;
pub mod facts;
pub mod initrd;
pub mod passwords;
pub mod processes;
pub mod reboot;
pub mod ssh;
