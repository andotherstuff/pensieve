//! Evidence-preserving recovery for a terminally truncated notepack segment.

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CanonicalEvent, Error, Result};

/// Format identifier for a terminal-truncation salvage report.
pub const NOTEPACK_SALVAGE_FORMAT: &str = "pensieve.notepack-salvage.v1";
/// Complete framed notepack prefix retained by a salvage bundle.
pub const SALVAGED_SEGMENT_NAME: &str = "salvaged.notepack";
/// Exact decompressed bytes of the incomplete terminal frame.
pub const TRUNCATED_TAIL_NAME: &str = "truncated-tail.bin";
/// Canonical evidence report within a salvage bundle.
pub const SALVAGE_REPORT_NAME: &str = "report.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SalvagePayload {
    format: String,
    source_name: String,
    source_bytes: u64,
    source_sha256: String,
    max_event_bytes: u64,
    complete_frames: u64,
    valid_events: u64,
    rejected_events: u64,
    truncated_frame_index: u64,
    declared_frame_bytes: Option<u64>,
    retained_tail_bytes: u64,
    salvaged_segment_bytes: u64,
    salvaged_segment_sha256: String,
    truncated_tail_sha256: String,
}

/// Content-addressed report for one atomically published salvage bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SalvageReport {
    /// SHA-256 identity of the compact canonical report payload.
    pub report_id: String,
    #[serde(flatten)]
    payload: SalvagePayload,
}

impl SalvageReport {
    /// Original source filename.
    pub fn source_name(&self) -> &str {
        &self.payload.source_name
    }

    /// Original source SHA-256.
    pub fn source_sha256(&self) -> &str {
        &self.payload.source_sha256
    }

    /// Number of structurally complete frames retained.
    pub fn complete_frames(&self) -> u64 {
        self.payload.complete_frames
    }

    /// Number of complete frames that passed canonical event validation.
    pub fn valid_events(&self) -> u64 {
        self.payload.valid_events
    }

    /// Number of complete frames retained for later quarantine.
    pub fn rejected_events(&self) -> u64 {
        self.payload.rejected_events
    }

    /// Zero-based index of the incomplete terminal frame.
    pub fn truncated_frame_index(&self) -> u64 {
        self.payload.truncated_frame_index
    }

    /// SHA-256 of the complete framed prefix.
    pub fn salvaged_segment_sha256(&self) -> &str {
        &self.payload.salvaged_segment_sha256
    }

    /// Exact source bytes observed during salvage.
    pub fn source_bytes(&self) -> u64 {
        self.payload.source_bytes
    }

    /// Verify report structure, accounting, hashes, and content identity.
    pub fn validate(&self) -> Result<()> {
        if self.payload.format != NOTEPACK_SALVAGE_FORMAT {
            return Err(Error::InvalidSalvage(format!(
                "unsupported report format {}",
                self.payload.format
            )));
        }
        let accounted = self
            .payload
            .valid_events
            .checked_add(self.payload.rejected_events)
            .ok_or_else(|| Error::InvalidSalvage("event accounting overflows".to_owned()))?;
        if self.payload.complete_frames != accounted {
            return Err(Error::InvalidSalvage(
                "complete-frame accounting does not reconcile".to_owned(),
            ));
        }
        if self.payload.truncated_frame_index != self.payload.complete_frames {
            return Err(Error::InvalidSalvage(
                "truncated frame does not immediately follow the complete prefix".to_owned(),
            ));
        }
        for (field, value) in [
            ("source_sha256", &self.payload.source_sha256),
            (
                "salvaged_segment_sha256",
                &self.payload.salvaged_segment_sha256,
            ),
            ("truncated_tail_sha256", &self.payload.truncated_tail_sha256),
        ] {
            if value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(Error::InvalidSalvage(format!(
                    "{field} is not lowercase SHA-256"
                )));
            }
        }
        let expected = content_id(&self.payload)?;
        if self.report_id != expected {
            return Err(Error::InvalidSalvage(format!(
                "report identity mismatch: expected {expected}, found {}",
                self.report_id
            )));
        }
        Ok(())
    }
}

/// Read and fully validate a canonical salvage report.
pub fn read_salvage_report(path: impl AsRef<Path>) -> Result<SalvageReport> {
    let bytes = fs::read(path)?;
    let report: SalvageReport =
        serde_json::from_slice(&bytes).map_err(|error| Error::InvalidSalvage(error.to_string()))?;
    report.validate()?;
    if bytes != canonical_json(&report)? {
        return Err(Error::InvalidSalvage(
            "report JSON is not canonically encoded".to_owned(),
        ));
    }
    Ok(report)
}

/// Preserve every complete frame and the exact incomplete terminal bytes.
///
/// The destination is an atomically renamed directory containing a plain
/// framed notepack prefix, the incomplete decompressed tail, and a canonical
/// content-addressed report. Complete invalid frames remain in the salvaged
/// segment so the normal campaign can quarantine them. A complete input is not
/// a salvage case and is rejected without creating output.
pub fn salvage_truncated_segment(
    input: impl AsRef<Path>,
    output_directory: impl AsRef<Path>,
    max_event_bytes: usize,
) -> Result<SalvageReport> {
    let input = input.as_ref();
    let output_directory = output_directory.as_ref();
    if output_directory.exists() {
        return Err(Error::OutputExists {
            path: output_directory.to_owned(),
        });
    }
    let source_name = input
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::InvalidSalvage("source filename is not UTF-8".to_owned()))?
        .to_owned();
    let source_bytes = input.metadata()?.len();
    let source_sha256 = sha256_file(input)?;
    let parent = output_directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".notepack-salvage-")
        .tempdir_in(parent)?;
    let salvaged_path = temporary.path().join(SALVAGED_SEGMENT_NAME);
    let tail_path = temporary.path().join(TRUNCATED_TAIL_NAME);
    let report_path = temporary.path().join(SALVAGE_REPORT_NAME);
    let mut salvaged = File::create(&salvaged_path)?;
    let mut reader = open_segment(input)?;
    let scan = copy_complete_prefix(&mut reader, &mut salvaged, &tail_path, max_event_bytes)?;
    salvaged.flush()?;
    salvaged.sync_all()?;
    File::open(&tail_path)?.sync_all()?;

    let payload = SalvagePayload {
        format: NOTEPACK_SALVAGE_FORMAT.to_owned(),
        source_name,
        source_bytes,
        source_sha256,
        max_event_bytes: to_u64(max_event_bytes, "max_event_bytes")?,
        complete_frames: to_u64(scan.complete_frames, "complete_frames")?,
        valid_events: to_u64(scan.valid_events, "valid_events")?,
        rejected_events: to_u64(scan.rejected_events, "rejected_events")?,
        truncated_frame_index: to_u64(scan.complete_frames, "truncated_frame_index")?,
        declared_frame_bytes: scan
            .declared_frame_bytes
            .map(|value| to_u64(value, "declared_frame_bytes"))
            .transpose()?,
        retained_tail_bytes: tail_path.metadata()?.len(),
        salvaged_segment_bytes: salvaged_path.metadata()?.len(),
        salvaged_segment_sha256: sha256_file(&salvaged_path)?,
        truncated_tail_sha256: sha256_file(&tail_path)?,
    };
    let report = SalvageReport {
        report_id: content_id(&payload)?,
        payload,
    };
    report.validate()?;
    let mut report_file = File::create(&report_path)?;
    report_file.write_all(&canonical_json(&report)?)?;
    report_file.sync_all()?;
    File::open(temporary.path())?.sync_all()?;

    let temporary_path = temporary.keep();
    if let Err(error) = fs::rename(&temporary_path, output_directory) {
        let _ = fs::remove_dir_all(&temporary_path);
        return Err(error.into());
    }
    File::open(parent)?.sync_all()?;
    Ok(report)
}

#[derive(Debug)]
struct PrefixScan {
    complete_frames: usize,
    valid_events: usize,
    rejected_events: usize,
    declared_frame_bytes: Option<usize>,
}

fn copy_complete_prefix(
    reader: &mut impl Read,
    salvaged: &mut impl Write,
    tail_path: &Path,
    max_event_bytes: usize,
) -> Result<PrefixScan> {
    let mut complete_frames = 0usize;
    let mut valid_events = 0usize;
    let mut rejected_events = 0usize;
    loop {
        let mut length_bytes = [0u8; 4];
        let length_bytes_read = read_until_eof(reader, &mut length_bytes)?;
        if length_bytes_read == 0 {
            return Err(Error::SegmentNotTruncated);
        }
        if length_bytes_read < length_bytes.len() {
            fs::write(tail_path, &length_bytes[..length_bytes_read])?;
            return Ok(PrefixScan {
                complete_frames,
                valid_events,
                rejected_events,
                declared_frame_bytes: None,
            });
        }
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > max_event_bytes {
            return Err(Error::FrameTooLarge {
                length,
                limit: max_event_bytes,
            });
        }
        let mut payload = vec![0u8; length];
        let payload_bytes_read = read_until_eof(reader, &mut payload)?;
        if payload_bytes_read < length {
            let mut tail = File::create(tail_path)?;
            tail.write_all(&length_bytes)?;
            tail.write_all(&payload[..payload_bytes_read])?;
            tail.flush()?;
            return Ok(PrefixScan {
                complete_frames,
                valid_events,
                rejected_events,
                declared_frame_bytes: Some(length),
            });
        }

        salvaged.write_all(&length_bytes)?;
        salvaged.write_all(&payload)?;
        if CanonicalEvent::from_notepack(&payload).is_ok() {
            valid_events += 1;
        } else {
            rejected_events += 1;
        }
        complete_frames += 1;
    }
}

fn read_until_eof(reader: &mut impl Read, destination: &mut [u8]) -> Result<usize> {
    let mut read = 0usize;
    while read < destination.len() {
        match reader.read(&mut destination[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(read)
}

fn open_segment(path: &Path) -> Result<Box<dyn Read>> {
    let file = File::open(path)?;
    if path.extension().is_some_and(|extension| extension == "gz") {
        Ok(Box::new(GzDecoder::new(BufReader::new(file))))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| Error::InvalidSalvage(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn content_id(value: &impl Serialize) -> Result<String> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| Error::InvalidSalvage(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn to_u64(value: usize, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::InvalidSalvage(format!("{field} cannot be represented as u64")))
}
