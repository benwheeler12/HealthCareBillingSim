//! Healthcare billing lifecycle simulation (Brace Health take-home).
//! See DESIGN.md for the converged design; module dependency rules:
//! `sim` and `biller` never import each other — shared types live in `domain`.

pub mod biller;
pub mod domain;
pub mod ledger;
pub mod sim;
