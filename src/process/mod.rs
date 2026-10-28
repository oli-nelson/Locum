pub mod pool;
pub mod spawn;
pub mod supervisor;

pub use spawn::{resolve_executable, CommandBuilder};
pub use supervisor::{ChildGuard, LineReader, StderrTail};
