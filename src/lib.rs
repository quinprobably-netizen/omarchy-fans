//! omarchy-fans: fan curves for hwmon PWM fans, built for the Omarchy shell.
//!
//! * `omarchy-fand` — root daemon (control loop, calibration, socket API)
//! * `omarchy-fans` — CLI used by you and by the shell plugin

pub mod calibrate;
pub mod curve;
pub mod daemon;
pub mod fan;
pub mod gpufan;
pub mod hwmon;
pub mod profiles;
pub mod sensor;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
