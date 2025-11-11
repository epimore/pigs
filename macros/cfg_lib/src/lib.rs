pub mod util;
pub mod conf;
extern crate cfg_macro;
pub use cfg_macro::conf as conf;

pub struct CliBasic {
    pub name:&'static str,
    pub version:&'static str,
    pub author:&'static str,
    pub about:&'static str,
}
#[macro_export]
macro_rules! default_cli_basic {
    () => {
        CliBasic {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            author: env!("CARGO_PKG_AUTHORS"),
            about: env!("CARGO_PKG_DESCRIPTION"),
        }
    };
}