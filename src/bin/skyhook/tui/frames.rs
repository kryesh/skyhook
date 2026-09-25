//! Frame output and pacing. Frames render into memory and a dedicated thread writes
//! them to the terminal, so a terminal that reads slowly holds frames back instead of
//! blocking the event loop.
use std::{
    cell::RefCell,
    io::{self, Write},
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};
use tokio::time::Instant;

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "terminal writer stopped")
}

/// The terminal's output stream. Writes collect in memory until the event loop
/// sends them, so a frame reaches the terminal in one write however often its
/// renderer flushes.
pub struct Output(Rc<RefCell<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The event loop's handle on the writer thread.
pub struct Writer {
    buffer: Rc<RefCell<Vec<u8>>>,
    chunks: mpsc::Sender<Vec<u8>>,
    /// Chunks sent that the terminal has not taken yet.
    outstanding: usize,
    written: tokio::sync::mpsc::UnboundedReceiver<io::Result<()>>,
    thread: thread::JoinHandle<()>,
}
impl Writer {
    pub fn spawn(mut terminal: impl Write + Send + 'static) -> (Output, Self) {
        let (chunks, received) = mpsc::channel::<Vec<u8>>();
        let (done, written) = tokio::sync::mpsc::unbounded_channel();
        let thread = thread::spawn(move || {
            for chunk in received {
                let result = terminal.write_all(&chunk).and_then(|()| terminal.flush());
                let failed = result.is_err();
                // At shutdown nobody waits for completions, but queued output still
                // reaches the terminal.
                let _ = done.send(result);
                if failed {
                    break;
                }
            }
        });
        let buffer = Rc::default();
        let writer = Self {
            buffer: Rc::clone(&buffer),
            chunks,
            outstanding: 0,
            written,
            thread,
        };
        (Output(buffer), writer)
    }
    /// Hand everything written since the last send to the terminal, in one write.
    pub fn send(&mut self) -> io::Result<()> {
        let chunk = std::mem::take(&mut *self.buffer.borrow_mut());
        if chunk.is_empty() {
            return Ok(());
        }
        self.outstanding += 1;
        self.chunks.send(chunk).map_err(|_| stopped())
    }
    /// Output has been sent that the terminal has not taken yet.
    pub fn busy(&self) -> bool {
        self.outstanding > 0
    }
    /// The next sent chunk reached the terminal. Cancel-safe.
    pub async fn written(&mut self) -> io::Result<()> {
        let result = self.written.recv().await.unwrap_or_else(|| Err(stopped()));
        self.outstanding -= 1;
        result
    }
    /// Send what is left and wait for it to reach the terminal.
    pub fn join(mut self) -> io::Result<()> {
        let sent = self.send();
        drop(self.chunks);
        let _ = self.thread.join();
        sent
    }
}

/// When a dirty frame may be drawn. Input paints as soon as the writer is free;
/// anything else is also held to a 50% render duty cycle. There is no fixed frame
/// rate: a slow terminal holds frames back through the writer, a slow render
/// through the budget, and nothing is drawn while nothing changes.
#[derive(Default)]
pub struct Pacer {
    input: bool,
    last: Option<(Instant, Duration)>,
}
impl Pacer {
    /// Input arrived: the next frame answers it without waiting out the budget.
    pub fn input(&mut self) {
        self.input = true;
    }
    /// The earliest the next frame may start, or `None` for now.
    pub fn due(&self) -> Option<Instant> {
        let (start, render) = self.last.filter(|_| !self.input)?;
        Some(start + render * 2)
    }
    pub fn drawn(&mut self, start: Instant, render: Duration) {
        self.last = Some((start, render));
        self.input = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_frames_keep_to_the_budget_and_input_skips_it() {
        let mut pacer = Pacer::default();
        assert_eq!(pacer.due(), None, "the first frame is immediate");
        let start = Instant::now();
        let render = Duration::from_millis(3);
        pacer.drawn(start, render);
        assert_eq!(pacer.due(), Some(start + Duration::from_millis(6)));
        pacer.input();
        assert_eq!(pacer.due(), None);
        pacer.drawn(start + render * 2, render);
        assert_eq!(pacer.due(), Some(start + Duration::from_millis(12)));
    }

    #[tokio::test]
    async fn output_reaches_the_terminal_in_send_order_and_releases_the_writer() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let (mut output, mut writer) = Writer::spawn(sink.clone());
        output.write_all(b"frame").unwrap();
        output.flush().unwrap();
        assert!(!writer.busy(), "a renderer's flush sends nothing");
        writer.send().unwrap();
        output.write_all(b" clipboard").unwrap();
        writer.send().unwrap();
        assert!(writer.busy());
        for _ in 0..2 {
            let written = tokio::time::timeout(Duration::from_secs(5), writer.written());
            written.await.expect("written in time").unwrap();
        }
        assert!(!writer.busy());
        output.write_all(b" restore").unwrap();
        writer.join().unwrap();
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            b"frame clipboard restore"
        );
    }
}
