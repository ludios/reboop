// Model-output: Claude Opus 5.5

//! Primitives for carefully rebooting NixOS machines: see doc/spec.md.

pub mod boot;
pub mod bounce;
pub mod btrfs;
pub mod check;
pub mod child;
pub mod config;
pub mod deadline;
pub mod facts;
pub mod human;
pub mod initrd;
pub mod luks_password;
pub mod passwords;
pub mod preflight;
pub mod processes;
pub mod reboot;
pub mod ssh;
