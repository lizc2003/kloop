use std::fmt;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;

use sha2::Digest as _;

use crate::file_state::FileVersion;

const STREAM_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct BoundedRead {
    pub bytes: Vec<u8>,
    pub metadata: std::fs::Metadata,
    pub version: FileVersion,
}

pub(crate) struct FingerprintedFile {
    pub metadata: std::fs::Metadata,
    pub version: FileVersion,
}

#[derive(Debug)]
pub(crate) enum FileReadError {
    TooLarge { actual: u64, limit: usize },
    Changed,
    Io(std::io::Error),
}

impl FileReadError {
    pub(crate) fn too_large(&self) -> Option<(u64, usize)> {
        match self {
            Self::TooLarge { actual, limit } => Some((*actual, *limit)),
            Self::Changed | Self::Io(_) => None,
        }
    }
}

impl fmt::Display for FileReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { actual, limit } => {
                write!(
                    formatter,
                    "file is {actual} bytes, over the {limit} byte limit"
                )
            }
            Self::Changed => write!(formatter, "file changed while it was being read"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FileReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLarge { .. } | Self::Changed => None,
        }
    }
}

impl From<std::io::Error> for FileReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn read_bounded(
    file: &mut std::fs::File,
    limit: usize,
) -> Result<BoundedRead, FileReadError> {
    file.seek(SeekFrom::Start(0))?;
    let before = file.metadata()?;
    if before.len() > limit as u64 {
        return Err(FileReadError::TooLarge {
            actual: before.len(),
            limit,
        });
    }

    let read_limit = limit
        .checked_add(1)
        .expect("file read limit must leave room for a growth sentinel");
    let mut bytes = Vec::with_capacity(read_limit.min(before.len() as usize + 1));
    {
        let mut bounded = file.take(read_limit as u64);
        bounded.read_to_end(&mut bytes)?;
    }
    let after = file.metadata()?;
    if !FileVersion::from_fingerprint([0; 32], &before).metadata_matches(&after) {
        return Err(FileReadError::Changed);
    }
    if bytes.len() > limit {
        return Err(FileReadError::TooLarge {
            actual: after.len().max(bytes.len() as u64),
            limit,
        });
    }

    let fingerprint = sha2::Sha256::digest(&bytes).into();
    Ok(BoundedRead {
        bytes,
        metadata: after,
        version: FileVersion::from_fingerprint(fingerprint, &before),
    })
}

pub(crate) fn fingerprint_file(
    file: &mut std::fs::File,
) -> Result<FingerprintedFile, FileReadError> {
    file.seek(SeekFrom::Start(0))?;
    let before = file.metadata()?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    let version = FileVersion::from_fingerprint(hasher.finalize().into(), &before);
    if !version.metadata_matches(&after) {
        return Err(FileReadError::Changed);
    }
    Ok(FingerprintedFile {
        metadata: after,
        version,
    })
}

pub(crate) fn file_contents_equal(
    file: &mut std::fs::File,
    expected: &[u8],
) -> Result<(bool, std::fs::Metadata), FileReadError> {
    file.seek(SeekFrom::Start(0))?;
    let before = file.metadata()?;
    if before.len() != expected.len() as u64 {
        return Ok((false, before));
    }

    let mut offset = 0usize;
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    let mut equal = true;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if !expected_chunk_matches(expected, offset, &buffer[..read]) {
            equal = false;
        }
        offset = offset.saturating_add(read);
    }
    let after = file.metadata()?;
    if !FileVersion::from_fingerprint([0; 32], &before).metadata_matches(&after) {
        return Err(FileReadError::Changed);
    }
    Ok((equal && offset == expected.len(), after))
}

fn expected_chunk_matches(expected: &[u8], offset: usize, actual: &[u8]) -> bool {
    expected
        .get(offset..)
        .and_then(|remaining| remaining.get(..actual.len()))
        == Some(actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_file(tag: &str, bytes: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("kloop-file-io-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        (path, file)
    }

    #[test]
    fn bounded_read_accepts_limit_and_rejects_limit_plus_one() {
        let (path, mut file) = temp_file("boundary", b"12345");
        assert_eq!(read_bounded(&mut file, 5).unwrap().bytes, b"12345");
        let error = read_bounded(&mut file, 4).unwrap_err();
        assert_eq!(error.too_large(), Some((5, 4)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fingerprint_and_equality_stream_without_changing_content() {
        let bytes = vec![b'x'; STREAM_BUFFER_BYTES * 2 + 7];
        let (path, mut file) = temp_file("stream", &bytes);
        let snapshot = fingerprint_file(&mut file).unwrap();
        assert!(snapshot.version.matches(&bytes, &snapshot.metadata));
        assert!(file_contents_equal(&mut file, &bytes).unwrap().0);
        let mut different = bytes.clone();
        different[STREAM_BUFFER_BYTES] = b'y';
        assert!(!file_contents_equal(&mut file, &different).unwrap().0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn chunk_comparison_rejects_growth_without_slicing_past_expected() {
        assert!(expected_chunk_matches(b"abcd", 2, b"cd"));
        assert!(!expected_chunk_matches(b"abcd", 3, b"de"));
        assert!(!expected_chunk_matches(b"abcd", 5, b"x"));
    }

    #[test]
    fn growth_past_limit_is_detected_by_cap_plus_one() {
        let (path, mut reader) = temp_file("growth", b"1234");
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(b"56").unwrap();
        writer.sync_all().unwrap();
        let error = read_bounded(&mut reader, 5).unwrap_err();
        assert_eq!(error.too_large(), Some((6, 5)));
        let _ = std::fs::remove_file(path);
    }
}
