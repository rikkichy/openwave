pub mod calibration;
pub mod effects;
pub mod health;
pub mod model;
pub mod profiles;
pub mod protocol;
pub mod routing;
pub mod scenes;

pub const VERSION: &str = env!("OPENWAVE_VERSION");
pub const RUST_COMPILER: &str = env!("OPENWAVE_BUILD_RUSTC");
pub const BUILD_TARGET: &str = env!("OPENWAVE_BUILD_TARGET");
