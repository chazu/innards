//! Pipe I/O cannot block the terminal thread. The only queues are bounded by
//! both record count and record size; overflow backpressures the producer.
use super::protocol::{FRAME_BYTES, QUEUE_FRAMES};
use anyhow::{Result, ensure};
use std::{
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
};
#[derive(Default)]
pub struct Traffic {
    pub received: AtomicUsize,
    pub sent: AtomicUsize,
    pub queued: AtomicUsize,
    pub peak: AtomicUsize,
}
pub struct Transport {
    pub input: Receiver<Result<Vec<u8>>>,
    output: SyncSender<Vec<u8>>,
    pub traffic: Arc<Traffic>,
    pub failed: Receiver<()>,
}
impl Transport {
    pub fn new(input: impl Read + Send + 'static, output: impl Write + Send + 'static) -> Self {
        let (incoming, input_rx) = mpsc::sync_channel(QUEUE_FRAMES);
        let (outgoing, output_rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_FRAMES);
        let (failure, failed) = mpsc::channel();
        let traffic = Arc::new(Traffic::default());
        let stats = traffic.clone();
        thread::spawn(move || {
            let mut input = BufReader::new(input);
            loop {
                let mut frame = Vec::new();
                let result = (&mut input)
                    .take((FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut frame);
                let result: Result<Vec<u8>> = match result {
                    Ok(0) => break,
                    Ok(_) => {
                        stats.received.fetch_add(frame.len(), Ordering::Relaxed);
                        if frame.len() > FRAME_BYTES || frame.last() != Some(&b'\n') {
                            Err(anyhow::anyhow!("oversized or incomplete UI frame"))
                        } else {
                            Ok(frame)
                        }
                    }
                    Err(e) => Err(e.into()),
                };
                let failed = result.is_err();
                if incoming.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let stats = traffic.clone();
        thread::spawn(move || {
            let mut output = output;
            while let Ok(frame) = output_rx.recv() {
                let count = frame.len();
                let ok = output
                    .write_all(&frame)
                    .and_then(|_| output.flush())
                    .is_ok();
                stats.queued.fetch_sub(count, Ordering::Relaxed);
                if !ok {
                    let _ = failure.send(());
                    break;
                }
                stats.sent.fetch_add(count, Ordering::Relaxed);
            }
        });
        Self {
            input: input_rx,
            output: outgoing,
            traffic,
            failed,
        }
    }
    pub fn send(&self, bytes: Vec<u8>) -> Result<bool> {
        ensure!(bytes.len() <= FRAME_BYTES, "outgoing frame too large");
        let size = bytes.len();
        let queued = self.traffic.queued.fetch_add(size, Ordering::Relaxed) + size;
        match self.output.try_send(bytes) {
            Ok(()) => {
                self.traffic.peak.fetch_max(queued, Ordering::Relaxed);
                Ok(true)
            }
            Err(TrySendError::Full(_)) => {
                self.traffic.queued.fetch_sub(size, Ordering::Relaxed);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.traffic.queued.fetch_sub(size, Ordering::Relaxed);
                Err(anyhow::anyhow!("UI output disconnected"))
            }
        }
    }
}
