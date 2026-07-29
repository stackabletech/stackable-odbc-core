//! Generic implementations of the ODBC FFI entry points, one submodule per
//! functional area. The `forward_ffi!` macro generates the C ABI wrappers that
//! call into these.

pub mod bind;
pub mod connect;
pub mod connect_attr;
pub mod cursor;
pub mod desc;
pub mod diag;
pub mod env;
pub mod execute;
pub mod fetch;
pub mod handle;
pub mod info;
pub mod metadata;
pub mod params;
pub mod setup;
pub mod stmt_attr;
pub mod tran;
