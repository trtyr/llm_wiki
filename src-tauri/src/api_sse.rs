//! SSE transport primitives for the streaming chat endpoint.

use std::io::{Cursor, Read};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use serde_json::Value;

pub(crate) struct SseReader {
    receiver: Receiver<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

impl SseReader {
    pub(crate) fn new(receiver: Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            current: Cursor::new(Vec::new()),
        }
    }
}

impl Read for SseReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.current.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }
            match self.receiver.recv() {
                Ok(chunk) => self.current = Cursor::new(chunk),
                Err(_) => return Ok(0),
            }
        }
    }
}

pub(crate) fn sse_frame(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {}\n\n", data).into_bytes()
}

pub(crate) enum SseSendResult {
    Sent,
    Full,
    Disconnected,
}

pub(crate) fn try_send_sse(sender: &SyncSender<Vec<u8>>, frame: Vec<u8>) -> SseSendResult {
    match sender.try_send(frame) {
        Ok(()) => SseSendResult::Sent,
        Err(mpsc::TrySendError::Full(_)) => SseSendResult::Full,
        Err(mpsc::TrySendError::Disconnected(_)) => SseSendResult::Disconnected,
    }
}

pub(crate) fn send_terminal_sse(sender: SyncSender<Vec<u8>>, frame: Vec<u8>) {
    // A terminal frame must not be dropped when the bounded queue is briefly
    // full, but blocking a Tokio worker would stall unrelated app work. Use a
    // short-lived writer thread; send unblocks when the socket drains or the
    // request reader is dropped after a disconnect.
    thread::spawn(move || {
        let _ = sender.send(frame);
    });
}
