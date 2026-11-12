use serde::de::IgnoredAny;
use std::io::{self, BufReader, Read};

// IgnoredAny checks syntax without collecting values. Its nesting stack still grows with
// depth, and skipped strings do not validate UTF-8. Check both while reading the bytes.
pub(super) fn valid_record(reader: impl Read) -> io::Result<bool> {
    let checked = CheckedJson {
        reader,
        depth: 0,
        quoted: false,
        escaped: false,
        utf8_tail: Vec::with_capacity(4),
    };
    match serde_json::from_reader::<_, IgnoredAny>(BufReader::new(checked)) {
        Ok(_) => Ok(true),
        Err(error)
            if error.is_io() && error.io_error_kind() != Some(io::ErrorKind::InvalidData) =>
        {
            Err(error.into())
        }
        Err(_) => Ok(false),
    }
}

struct CheckedJson<R> {
    reader: R,
    depth: usize,
    quoted: bool,
    escaped: bool,
    utf8_tail: Vec<u8>,
}

impl<R: Read> Read for CheckedJson<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buf)?;
        let mut bytes = &buf[..n];
        if !self.utf8_tail.is_empty() {
            let expected = self.utf8_tail[0].leading_ones() as usize;
            let needed = bytes.len().min(expected - self.utf8_tail.len());
            self.utf8_tail.extend_from_slice(&bytes[..needed]);
            bytes = &bytes[needed..];
            if self.utf8_tail.len() == expected {
                std::str::from_utf8(&self.utf8_tail).map_err(|_| io::ErrorKind::InvalidData)?;
                self.utf8_tail.clear();
            } else if n == 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.utf8_tail
                .extend_from_slice(&bytes[error.valid_up_to()..]);
        }
        for &byte in &buf[..n] {
            if self.escaped {
                self.escaped = false;
            } else if self.quoted {
                self.escaped = byte == b'\\';
                self.quoted = byte != b'"';
            } else {
                match byte {
                    b'"' => self.quoted = true,
                    b'[' | b'{' => {
                        self.depth += 1;
                        if self.depth > 128 {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                    }
                    b']' | b'}' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::valid_record;

    #[test]
    fn skipped_records_check_utf8_across_buffers_and_limit_nesting_outside_strings() {
        let mut value = vec![b'"'];
        value.resize(8191, b'[');
        value.extend_from_slice("中\\\"🙂\"".as_bytes());
        assert!(valid_record(value.as_slice()).unwrap());
        value[8192] = 0xff;
        assert!(!valid_record(value.as_slice()).unwrap());
        for (depth, accepted) in [(128, true), (129, false)] {
            let value = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(valid_record(value.as_bytes()).unwrap(), accepted);
        }
    }
}
