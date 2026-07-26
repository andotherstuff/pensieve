//! Streaming framed-notepack input and atomic canonical file publication.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use flate2::read::GzDecoder;

use crate::{CanonicalEvent, Error, Result, write_canonical_events};

/// Default maximum accepted size of one framed notepack event.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Counts and byte sizes from one completed segment conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversionSummary {
    /// Input notepack frames read and validated.
    pub input_events: usize,
    /// Canonical rows written after in-file deduplication.
    pub output_rows: usize,
    /// Duplicate input IDs removed.
    pub duplicate_events: usize,
    /// Invalid event frames excluded and written to an explicit reject segment.
    pub rejected_events: usize,
    /// Parquet row groups written.
    pub row_groups: usize,
    /// Compressed or uncompressed input file size on disk.
    pub input_file_bytes: u64,
    /// Completed Parquet file size on disk.
    pub output_file_bytes: u64,
}

/// One invalid event frame retained verbatim during a diagnostic scan.
#[derive(Debug)]
pub struct RejectedFrame {
    /// Zero-based frame index in the input segment.
    pub frame_index: usize,
    /// Original notepack payload without its length prefix.
    pub payload: Vec<u8>,
    /// Decoding or canonical validation failure.
    pub error: Error,
}

/// Valid rows and invalid frames found by a full diagnostic segment scan.
#[derive(Debug)]
pub struct SegmentScan {
    /// Canonically valid events.
    pub events: Vec<CanonicalEvent>,
    /// Invalid frames, preserved in input order.
    pub rejected: Vec<RejectedFrame>,
}

/// Read and validate every frame from a length-prefixed notepack stream.
///
/// The stream framing is `[u32 little-endian length][notepack payload]`.
/// Payloads are decoded directly into typed canonical rows without JSON.
pub fn read_framed_notepack<R: Read>(
    mut reader: R,
    max_event_bytes: usize,
) -> Result<Vec<CanonicalEvent>> {
    let mut rows = Vec::new();
    let mut length_bytes = [0u8; 4];
    let mut frame_index = 0usize;

    loop {
        match reader.read(&mut length_bytes[..1]) {
            Ok(0) => break,
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read cannot return more than one byte"),
            Err(error) => return Err(error.into()),
        }
        reader
            .read_exact(&mut length_bytes[1..])
            .map_err(|error| truncated_or_io(error, frame_index))?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > max_event_bytes {
            return Err(Error::FrameTooLarge {
                length,
                limit: max_event_bytes,
            });
        }

        let mut payload = vec![0; length];
        reader
            .read_exact(&mut payload)
            .map_err(|error| truncated_or_io(error, frame_index))?;
        rows.push(CanonicalEvent::from_notepack(&payload).map_err(|source| {
            Error::FrameValidation {
                frame_index,
                source: Box::new(source),
            }
        })?);
        frame_index += 1;
    }

    Ok(rows)
}

/// Scan a complete framed stream, retaining invalid event frames for quarantine.
///
/// Structural framing errors still abort because the next frame boundary
/// cannot be recovered safely.
pub fn scan_framed_notepack<R: Read>(mut reader: R, max_event_bytes: usize) -> Result<SegmentScan> {
    let mut events = Vec::new();
    let mut rejected = Vec::new();
    let mut length_bytes = [0u8; 4];
    let mut frame_index = 0usize;

    loop {
        match reader.read(&mut length_bytes[..1]) {
            Ok(0) => break,
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read cannot return more than one byte"),
            Err(error) => return Err(error.into()),
        }
        reader
            .read_exact(&mut length_bytes[1..])
            .map_err(|error| truncated_or_io(error, frame_index))?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > max_event_bytes {
            return Err(Error::FrameTooLarge {
                length,
                limit: max_event_bytes,
            });
        }

        let mut payload = vec![0; length];
        reader
            .read_exact(&mut payload)
            .map_err(|error| truncated_or_io(error, frame_index))?;
        match CanonicalEvent::from_notepack(&payload) {
            Ok(event) => events.push(event),
            Err(error) => rejected.push(RejectedFrame {
                frame_index,
                payload,
                error,
            }),
        }
        frame_index += 1;
    }

    Ok(SegmentScan { events, rejected })
}

/// Open and scan a plain or gzip-compressed framed notepack segment.
pub fn scan_segment(path: impl AsRef<Path>, max_event_bytes: usize) -> Result<SegmentScan> {
    scan_framed_notepack(open_segment(path.as_ref())?, max_event_bytes)
}

/// Atomically write rejected frames as a plain or gzip framed notepack segment.
pub fn write_rejected_segment(output: impl AsRef<Path>, rejected: &[RejectedFrame]) -> Result<()> {
    let output = output.as_ref();
    refuse_existing(output)?;
    write_rejects_atomically(output, rejected)
}

/// Convert one framed notepack segment into one atomically published V1 file.
///
/// Input gzip compression is detected by a `.gz` filename suffix. The output
/// path must not already exist. A temporary file in the destination directory
/// is synced and renamed only after the Parquet footer is complete.
pub fn convert_segment(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    max_event_bytes: usize,
) -> Result<ConversionSummary> {
    let input = input.as_ref();
    let output = output.as_ref();
    if output.exists() {
        return Err(Error::OutputExists {
            path: output.to_owned(),
        });
    }

    let input_file_bytes = input.metadata()?.len();
    let reader = open_segment(input)?;
    let rows = read_framed_notepack(reader, max_event_bytes)?;
    let input_events = rows.len();

    let (write_summary, output_file_bytes) = write_parquet_atomically(output, rows)?;

    Ok(ConversionSummary {
        input_events,
        output_rows: write_summary.output_rows,
        duplicate_events: write_summary.duplicate_events,
        rejected_events: 0,
        row_groups: write_summary.row_groups,
        input_file_bytes,
        output_file_bytes,
    })
}

/// Convert a segment while explicitly quarantining invalid event frames.
///
/// This mode is intended for auditing historical inputs. The reject segment
/// preserves each invalid notepack payload with the original framing, in input
/// order. It is written only when invalid frames exist. Both outputs refuse to
/// overwrite existing files.
pub fn convert_segment_quarantining_invalid(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    rejected_output: impl AsRef<Path>,
    max_event_bytes: usize,
) -> Result<ConversionSummary> {
    let input = input.as_ref();
    let output = output.as_ref();
    let rejected_output = rejected_output.as_ref();
    refuse_existing(output)?;
    refuse_existing(rejected_output)?;

    let input_file_bytes = input.metadata()?.len();
    let scan = scan_framed_notepack(open_segment(input)?, max_event_bytes)?;
    let input_events = scan.events.len() + scan.rejected.len();
    let rejected_events = scan.rejected.len();

    if !scan.rejected.is_empty() {
        write_rejects_atomically(rejected_output, &scan.rejected)?;
    }
    let (write_summary, output_file_bytes) = write_parquet_atomically(output, scan.events)?;

    Ok(ConversionSummary {
        input_events,
        output_rows: write_summary.output_rows,
        duplicate_events: write_summary.duplicate_events,
        rejected_events,
        row_groups: write_summary.row_groups,
        input_file_bytes,
        output_file_bytes,
    })
}

fn open_segment(path: &Path) -> Result<Box<dyn Read>> {
    let file = File::open(path)?;
    if path.extension().is_some_and(|extension| extension == "gz") {
        Ok(Box::new(GzDecoder::new(BufReader::new(file))))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

fn write_parquet_atomically(
    output: &Path,
    rows: Vec<CanonicalEvent>,
) -> Result<(crate::WriteSummary, u64)> {
    refuse_existing(output)?;
    let parent = parent_directory(output);
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let summary = write_canonical_events(temporary.as_file_mut(), rows)?;
    temporary.as_file_mut().sync_all()?;
    persist(temporary, output)?;
    Ok((summary, output.metadata()?.len()))
}

fn write_rejects_atomically(output: &Path, rejected: &[RejectedFrame]) -> Result<()> {
    let parent = parent_directory(output);
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if output
        .extension()
        .is_some_and(|extension| extension == "gz")
    {
        let mut gzip =
            flate2::write::GzEncoder::new(temporary.as_file_mut(), flate2::Compression::default());
        write_rejected_frames(&mut gzip, rejected)?;
        gzip.try_finish()?;
    } else {
        write_rejected_frames(temporary.as_file_mut(), rejected)?;
    }
    temporary.as_file_mut().sync_all()?;
    persist(temporary, output)
}

fn write_rejected_frames(writer: &mut impl Write, rejected: &[RejectedFrame]) -> Result<()> {
    for frame in rejected {
        let length = u32::try_from(frame.payload.len()).map_err(|_| Error::FrameTooLarge {
            length: frame.payload.len(),
            limit: u32::MAX as usize,
        })?;
        writer.write_all(&length.to_le_bytes())?;
        writer.write_all(&frame.payload)?;
    }
    writer.flush()?;
    Ok(())
}

fn refuse_existing(path: &Path) -> Result<()> {
    if path.exists() {
        Err(Error::OutputExists {
            path: path.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn persist(temporary: tempfile::NamedTempFile, output: &Path) -> Result<()> {
    let parent = parent_directory(output);
    temporary
        .persist_noclobber(output)
        .map_err(|error| Error::Io(error.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn truncated_or_io(error: std::io::Error, frame_index: usize) -> Error {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::TruncatedFrame { frame_index }
    } else {
        Error::Io(error)
    }
}
