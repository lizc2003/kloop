use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const BASE58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const ID_LEN: usize = 11;
const DIGEST_LEN: usize = 25;
const MAX_DIGEST: &str = "f5lxx1zz5pnorynqglhzmsp33";

pub fn fresh(prefix: &str) -> io::Result<String> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let suffix = encode::<8, ID_LEN>(bytes, BASE58);
    Ok(format!("{prefix}{suffix}"))
}

fn encode<const BYTES: usize, const DIGITS: usize>(
    mut bytes: [u8; BYTES],
    alphabet: &[u8],
) -> String {
    let base = alphabet.len() as u16;
    let mut encoded = [alphabet[0]; DIGITS];
    for digit in encoded.iter_mut().rev() {
        let mut remainder = 0_u16;
        for byte in &mut bytes {
            let value = remainder * 256 + u16::from(*byte);
            *byte = (value / base) as u8;
            remainder = value % base;
        }
        *digit = alphabet[usize::from(remainder)];
    }
    debug_assert!(bytes.iter().all(|byte| *byte == 0));
    String::from_utf8(encoded.to_vec()).expect("identity alphabets are ASCII")
}

pub fn encode_digest(bytes: [u8; 32]) -> String {
    let mut prefix = [0; 16];
    prefix.copy_from_slice(&bytes[..16]);
    encode::<16, DIGEST_LEN>(prefix, BASE36)
}

pub(crate) fn is_digest(value: &str) -> bool {
    value.len() == DIGEST_LEN
        && value.bytes().all(|byte| BASE36.contains(&byte))
        && value <= MAX_DIGEST
}

pub(crate) fn is_suffix(value: &str) -> bool {
    value.len() == ID_LEN && value.bytes().all(|byte| BASE58.contains(&byte))
}

pub(crate) fn create_file(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> io::Result<(String, PathBuf, File)> {
    create_file_with(dir, suffix, || fresh(prefix))
}

fn create_file_with(
    dir: &Path,
    suffix: &str,
    mut next: impl FnMut() -> io::Result<String>,
) -> io::Result<(String, PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    loop {
        let id = next()?;
        let path = dir.join(format!("{id}{suffix}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((id, path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn base58_preserves_all_64_bits_in_a_fixed_width_token() {
        assert_eq!(encode::<8, ID_LEN>([0; 8], BASE58), "11111111111");
        assert_eq!(encode::<8, ID_LEN>([255; 8], BASE58), "jpXCZedGfVQ");
        let id = fresh("bg-").unwrap();
        assert!(is_suffix(id.strip_prefix("bg-").unwrap()));
        assert!(!is_suffix("0123456789A"));
        assert!(!is_suffix("h7Kp2mV9Qx"));
        assert!(!is_suffix("h7Kp2mV9Qx4R"));
    }

    #[test]
    fn digest_encoding_preserves_the_first_128_bits_in_25_lowercase_base36_digits() {
        assert_eq!(encode_digest([0; 32]), "0".repeat(25));
        assert_eq!(encode_digest([255; 32]), MAX_DIGEST);
        let mut lowest_bit = [0; 32];
        lowest_bit[15] = 1;
        assert_eq!(encode_digest(lowest_bit), format!("{}1", "0".repeat(24)));
        let mut highest_bit = [0; 32];
        highest_bit[0] = 128;
        assert_eq!(encode_digest(highest_bit), "7ksyyizzkutudzbv8aqztecjk");
        assert!(is_digest(&"0".repeat(25)));
        assert!(is_digest(MAX_DIGEST));
        assert!(!is_digest("f5lxx1zz5pnorynqglhzmsp34"));
        assert!(!is_digest(&"0".repeat(24)));
        assert!(!is_digest(&"0".repeat(50)));
        assert!(!is_digest(&MAX_DIGEST.to_uppercase()));
    }

    #[test]
    fn digest_encoding_ignores_the_last_128_bits() {
        let prefix_only = [42; 32];
        let mut changed_tail = prefix_only;
        changed_tail[16..].fill(255);
        assert_eq!(encode_digest(prefix_only), encode_digest(changed_tail));
        changed_tail[15] ^= 1;
        assert_ne!(encode_digest(prefix_only), encode_digest(changed_tail));
    }

    #[test]
    fn an_existing_file_is_preserved_and_a_collision_retries() {
        let dir = std::env::temp_dir().join(fresh("kloop-resource-").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("taken.txt"), b"original").unwrap();
        let mut ids = ["taken", "fresh"].into_iter();
        let (id, path, mut file) =
            create_file_with(&dir, ".txt", || Ok(ids.next().unwrap().into())).unwrap();
        file.write_all(b"new").unwrap();
        assert_eq!(id, "fresh");
        assert_eq!(path, dir.join("fresh.txt"));
        assert_eq!(std::fs::read(dir.join("taken.txt")).unwrap(), b"original");
        assert_eq!(std::fs::read(path).unwrap(), b"new");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
