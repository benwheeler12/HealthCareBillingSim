//! Simulation adversary: clearinghouse + payers. Memoryless and reproducible —
//! pure functions of seed + config. Must never import `biller/`.

pub mod faults;
pub mod payer;
pub mod rng;
pub mod sim_truth;

pub use faults::FaultProfile;
pub use payer::{PayerConfig, default_payer_configs};
pub use rng::RngFactory;
