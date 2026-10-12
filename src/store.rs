//! An NDJSON sink for offloaded records, with the durability step the
//! acknowledgement depends on.
//!
//! This exists as one implementation rather than two because of what it is: the
//! `fsync` here is the step that makes an acknowledgement safe to send. Two
//! copies of that — one in the CLI, one in the TUI — is two places for the
//! ordering to drift, and the symptom of drift is silent data loss months later
//! rather than a failing test.
//!
//! One line of JSON per record, appended. NDJSON because an offload is a stream
//! that can stop at any point: a half-written array is not readable, and a
//! half-written NDJSON file is readable up to its last complete line.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::history::PendingChunk;
use crate::types::{Record, RejectedFrame};

/// Appends records to an NDJSON file, one JSON object per line.
#[derive(Debug)]
pub struct NdjsonSink {
    file: BufWriter<File>,
    path: PathBuf,
    records: u64,
    rejected: u64,
}

impl NdjsonSink {
    /// Open `path` for appending, creating it if needed.
    ///
    /// Appending rather than truncating: a second offload of the same night
    /// adds to what is there, and the duplicate records a replayed chunk
    /// produces are expected anyway — the protocol gives no exactly-once
    /// guarantee, so a consumer has to tolerate them regardless.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
            records: 0,
            rejected: 0,
        })
    }

    /// Where this sink is writing.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records written so far.
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Frames preserved for diagnostics — those that failed the envelope check.
    ///
    /// Undecoded *records* are not counted here: they are records, written like
    /// any other, carrying their own bytes.
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Write one chunk and make it durable.
    ///
    /// **Call this before acknowledging the chunk, and withhold the
    /// acknowledgement if it fails.** A successful acknowledgement lets the
    /// strap reclaim the history it just sent, so the ordering is the one thing
    /// this type exists to get right:
    ///
    /// ```text
    /// write records → write preserved frames → flush → fsync → ack
    /// ```
    ///
    /// `fsync`, not a plain flush: flushing moves bytes from this process to
    /// the kernel, which is not the same as surviving a power loss. It is one
    /// `fsync` per chunk — roughly every fifty records — so the cost is bounded
    /// rather than per-record.
    pub fn write_chunk(&mut self, chunk: &PendingChunk) -> Result<()> {
        // One line per record, undecoded ones included: those carry their
        // complete frame, so the bytes that could explain a decode failure land
        // inside the same `fsync` as the rows.
        for record in &chunk.records {
            self.write_line(record)?;
            self.records += 1;
        }
        self.sync()
    }

    /// Record a frame that failed its envelope check.
    ///
    /// Not synced on its own: these are diagnostics, and the next chunk's
    /// `fsync` covers them. Nothing is acknowledged on the strength of one.
    pub fn write_rejected(&mut self, frame: &RejectedFrame) -> Result<()> {
        self.write_line(frame)?;
        self.rejected += 1;
        Ok(())
    }

    /// Write a single record outside a chunk, without syncing.
    pub fn write_record(&mut self, record: &Record) -> Result<()> {
        self.write_line(record)?;
        self.records += 1;
        Ok(())
    }

    /// Flush and `fsync`.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(())
    }

    /// Flush, `fsync` and close.
    pub fn finish(mut self) -> Result<()> {
        self.sync()
    }

    fn write_line<T: serde::Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.file, value)?;
        self.file.write_all(b"\n")?;
        Ok(())
    }
}

/// Lowercase hex, for raw bytes in a JSON document.
///
/// A JSON array of 2,140 numbers is unreadable and four times the size; hex
/// round-trips through every tool a capture gets opened in.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_two_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xaa, 0xff]), "000faaff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn a_sink_appends_rather_than_truncating() {
        // A second offload of the same night adds to the file. Duplicates are
        // expected anyway — the protocol promises no exactly-once delivery.
        let dir = std::env::temp_dir().join(format!("whoop-rs-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("records.ndjson");
        let _ = std::fs::remove_file(&path);

        let mut sink = NdjsonSink::create(&path).unwrap();
        sink.write_rejected(&RejectedFrame {
            reason: crate::protocol::FrameRejectReason::PayloadChecksum,
            packet_type: Some(47),
            raw: vec![0xAA, 0x01],
        })
        .unwrap();
        sink.finish().unwrap();

        let mut again = NdjsonSink::create(&path).unwrap();
        again
            .write_rejected(&RejectedFrame {
                reason: crate::protocol::FrameRejectReason::HeaderChecksum,
                packet_type: None,
                raw: vec![0xAA, 0x02],
            })
            .unwrap();
        again.finish().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        // One complete JSON object per line, so a truncated run stays readable.
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("each line parses alone");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
