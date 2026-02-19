//! Dispatch thread — receives responses from worker, calls back into Haskell.

use flume::Receiver;

use crate::ffi::{CallResponseCallback, ResponseMessage};

pub fn run(response_rx: Receiver<ResponseMessage>, cb: CallResponseCallback) {
    while let Ok(ResponseMessage { mut token, body }) = response_rx.recv() {
        let bytes = body.to_bytes();
        unsafe {
            cb.respond(&mut token, bytes.as_ptr(), bytes.len());
        }
    }
}
