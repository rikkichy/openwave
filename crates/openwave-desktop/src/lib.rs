pub mod actions;
pub mod app;
pub mod icons;
pub mod tray;
pub mod ui;

pub type Submit = std::rc::Rc<dyn Fn(openwave_runtime::controller::AppCommand)>;

pub use openwave_core::VERSION;
