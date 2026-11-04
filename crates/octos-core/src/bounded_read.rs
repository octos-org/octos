//! Bounded synchronous reads for downloads and archive extraction.
//!
//! R-24: skill binary downloads and Office/ZIP unpack read responses and
//! archive entries into memory with no byte budget, so a hostile endpoint or
//! a zip bomb could exhaust memory/disk. `read_to_end_capped` retains at most
//! `cap` bytes and reports overflow; callers must fail loudly on overflow
//! (partial content is never a valid install).

use std::io::Read;

/// Cap on a skill binary download (R-24). Skill binaries are single
/// executables; 64 MiB covers the largest realistic toolchain binary while
/// bounding memory per download.
pub const SKILL_DOWNLOAD_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Cap on one extracted archive member (R-24): tar.gz skill binaries and
/// Office ZIP entries. Matches the download cap — a member larger than the
/// cap is treated as hostile/corrupt, not truncated.
pub const ARCHIVE_ENTRY_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Total cap across all members of one Office/ZIP unpack (R-24): guards the
/// many-small-entries case that a per-entry cap alone misses.
pub const ARCHIVE_TOTAL_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Max entry count for one Office/ZIP unpack (R-24): guards metadata-only
/// bombs (millions of tiny entries).
pub const ARCHIVE_MAX_ENTRIES: usize = 100_000;

/// Error returned when a stream exceeds its cap.
#[derive(Debug)]
pub struct CapExceeded {
    pub cap: usize,
}

impl std::fmt::Display for CapExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream exceeded the {}-byte cap", self.cap)
    }
}

impl std::error::Error for CapExceeded {}

/// Read `reader` to EOF, retaining at most `cap` bytes. On overflow returns
/// [`CapExceeded`] — the retained prefix is discarded by callers, never
/// written out. The stream is fully drained only up to `cap + 1` bytes (a
/// sync caller has no cancellation channel; the cap itself bounds memory).
pub fn read_to_end_capped<R: Read>(mut reader: R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut limited = reader.by_ref().take(cap as u64 + 1);
    limited.read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            CapExceeded { cap },
        ));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-24: content past the cap fails loudly instead of being retained.
    #[test]
    fn read_to_end_capped_rejects_overflow() {
        let data = vec![b'x'; 4096];
        let error =
            read_to_end_capped(&data[..], 2048).expect_err("past-cap content must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("2048"));
    }

    /// R-24: in-cap content (including exactly-at-cap) is retained whole.
    #[test]
    fn read_to_end_capped_accepts_in_cap_content() {
        let data = vec![b'y'; 2048];
        assert_eq!(read_to_end_capped(&data[..], 2048).unwrap(), data);
        let short = vec![b'z'; 100];
        assert_eq!(read_to_end_capped(&short[..], 2048).unwrap(), short);
    }
}
