//! 协作式取消：CPU 密集循环在边界处检查令牌，不强制终止 worker 线程。

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

pub fn check_cancelled(cancel: &AtomicBool) -> Result<(), Cancelled> {
    if cancel.load(Ordering::Relaxed) {
        Err(Cancelled)
    } else {
        Ok(())
    }
}
