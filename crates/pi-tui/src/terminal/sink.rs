//! Buffered stdout sink used during pure composition.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Collects backend output into a shared buffer so a transaction can flush once.
#[derive(Debug, Clone)]
pub struct FrameSink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Default for FrameSink {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSink {
    /// Create an empty sink that suppresses intermediate flushes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create a sink sharing an existing buffer slot.
    #[must_use]
    pub fn with_shared(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { buffer }
    }

    /// Shared buffer handle for draining after `CrosstermBackend` ownership.
    #[must_use]
    pub fn shared_buffer(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.buffer)
    }

    /// Borrow a snapshot of the buffered bytes.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Number of buffered bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.lock().map_or(0, |guard| guard.len())
    }

    /// Returns true when no bytes are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take ownership of the buffered bytes and clear the sink.
    #[must_use]
    pub fn take(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// Clear buffered bytes without returning them.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.buffer.lock() {
            guard.clear();
        }
    }

    /// Append raw bytes without going through the Write trait.
    pub fn extend_from_slice(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.buffer.lock() {
            guard.extend_from_slice(bytes);
        }
    }
}

impl Write for FrameSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .buffer
            .lock()
            .map_err(|_| io::Error::other("frame sink lock poisoned"))?;
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Stage-2 composition must never reach the outer terminal mid-transaction.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FrameSink;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn frame_sink_buffers_and_take_clears() -> std::io::Result<()> {
        let mut sink = FrameSink::new();
        sink.write_all(b"hello")?;
        sink.flush()?;
        assert_eq!(sink.bytes(), b"hello");
        let taken = sink.take();
        assert_eq!(taken, b"hello");
        assert!(sink.is_empty());
        Ok(())
    }

    #[test]
    fn shared_buffer_is_visible_across_clones() -> std::io::Result<()> {
        let sink = FrameSink::new();
        let shared = sink.shared_buffer();
        let mut clone = FrameSink::with_shared(Arc::clone(&shared));
        clone.write_all(b"abc")?;
        assert_eq!(sink.take(), b"abc");
        assert!(shared.lock().is_ok_and(|guard| guard.is_empty()));
        Ok(())
    }
}
