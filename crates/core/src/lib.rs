//! Core of tgsort: configuration, rule matching and safe file moving.

mod config;
mod mover;
mod rules;

pub use config::{Config, Rule, Source};
pub use mover::{execute, Outcome};
pub use rules::{is_ignorable, plan, plan_with_choice, AiChoice, Plan};
