//! Checks the recovered SQLite commit boundary against the WAL in the private database copy.

use std::{
    fmt,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Result, ensure};
use rusqlite::Connection;

/// The private SQLite snapshot cannot verify the native WAL commit boundary.
#[derive(Debug)]
pub struct UnverifiedSqliteWal(pub(crate) String);

impl fmt::Display for UnverifiedSqliteWal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQLite WAL commit boundary could not be verified ({})",
            self.0
        )
    }
}

impl std::error::Error for UnverifiedSqliteWal {}

const HEADER_BYTES: u64 = 32;
const FRAME_HEADER_BYTES: u64 = 24;
pub(super) fn verify(connection: &Connection, path: &Path) -> Result<()> {
    let mut wal = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let len = wal.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut header = [0; 32];
    wal.read_exact(&mut header)?;
    let (page_size, read, seed) = header_fields(&header)?;
    let words = header.as_chunks::<4>().0;
    let stride = FRAME_HEADER_BYTES + u64::from(page_size);
    ensure!(
        (len - HEADER_BYTES).is_multiple_of(stride),
        "incomplete SQLite WAL frame"
    );
    let (busy, committed) =
        connection.query_row("PRAGMA main.wal_checkpoint(NOOP)", [], |row| {
            Ok((row.get::<_, bool>(0)?, row.get::<_, u32>(1)?))
        })?;
    let prefix_end = HEADER_BYTES + u64::from(committed) * stride;
    ensure!(
        !busy && prefix_end <= len,
        "unverified SQLite WAL commit boundary"
    );
    if prefix_end == len {
        return Ok(());
    }

    // SQLite has already checked the committed prefix. Only its remaining bytes can be ambiguous.
    let mut running = seed;
    if committed > 0 {
        wal.seek(SeekFrom::Start(prefix_end - stride + 16))?;
        let mut saved = [0; 8];
        wal.read_exact(&mut saved)?;
        let pair = saved.as_chunks::<4>().0;
        running = [u32::from_be_bytes(pair[0]), u32::from_be_bytes(pair[1])];
    }
    wal.seek(SeekFrom::Start(prefix_end))?;
    let mut page = vec![0; usize::try_from(page_size)?];
    let suffix_frames = (len - prefix_end) / stride;
    let current = [words[4], words[5]];
    for index in 0..suffix_frames {
        let mut frame = [0; 24];
        wal.read_exact(&mut frame)?;
        let fields = frame.as_chunks::<4>().0;
        let salts = [fields[2], fields[3]];
        ensure!(fields[0] != [0; 4], "invalid SQLite WAL page number");
        if salts == current {
            ensure!(fields[1] == [0; 4], "unverified SQLite WAL commit");
        }
        wal.read_exact(&mut page)?;
        running = checksum(&frame[..8], read, running);
        running = checksum(&page, read, running);
        let matches =
            running[0].to_be_bytes() == fields[4] && running[1].to_be_bytes() == fields[5];
        if salts != current {
            // Frame checksums exclude the salts. A current checksum makes the apparent reset ambiguous.
            ensure!(
                !matches,
                "SQLite WAL frame has conflicting generation and checksum"
            );
            return verify_older_headers(
                &mut wal,
                suffix_frames - index,
                page_size,
                current,
                salts,
            );
        }
        ensure!(matches, "unverified SQLite WAL frame checksum");
    }
    Ok(())
}

type WordReader = fn([u8; 4]) -> u32;

fn header_fields(header: &[u8; 32]) -> Result<(u32, WordReader, [u32; 2])> {
    let words = header.as_chunks::<4>().0;
    let read = match u32::from_be_bytes(words[0]) {
        0x377f_0682 => u32::from_le_bytes,
        0x377f_0683 => u32::from_be_bytes,
        _ => anyhow::bail!("invalid SQLite WAL magic"),
    };
    let page_size = u32::from_be_bytes(words[2]);
    let seed = [u32::from_be_bytes(words[6]), u32::from_be_bytes(words[7])];
    ensure!(
        u32::from_be_bytes(words[1]) == 3_007_000
            && (512..=65_536).contains(&page_size)
            && page_size.is_power_of_two()
            && checksum(&header[..24], read, [0, 0]) == seed,
        "invalid SQLite WAL header"
    );
    Ok((page_size, read, seed))
}

/// Identifies each committed prefix by its header, frames, and page bytes, stopping
/// at the first frame SQLite recovery cannot use.
pub(super) fn commit_history(path: &Path, max_bytes: u64) -> Result<Vec<uuid::Uuid>> {
    use sha2::{Digest, Sha256};

    let file = File::open(path)?;
    let len = file.metadata()?.len();
    ensure!(len <= max_bytes, "WAL exceeds the snapshot byte limit");
    let mut wal = BufReader::new(file);
    let mut header = [0; 32];
    wal.read_exact(&mut header)?;
    let (page_size, read, mut running) = header_fields(&header)?;
    let stride = u64::from(page_size) + FRAME_HEADER_BYTES;
    let mut hash = Sha256::new();
    hash.update(header);
    let mut page = vec![0; usize::try_from(page_size)?];
    let mut commits = Vec::new();
    for _ in 0..(len - HEADER_BYTES) / stride {
        let mut frame = [0; 24];
        wal.read_exact(&mut frame)?;
        if frame[..4] == [0; 4] || frame[8..16] != header[16..24] {
            break;
        }
        wal.read_exact(&mut page)?;
        running = checksum(&frame[..8], read, running);
        running = checksum(&page, read, running);
        if frame[16..20] != running[0].to_be_bytes() || frame[20..24] != running[1].to_be_bytes() {
            break;
        }
        hash.update(frame);
        hash.update(&page);
        if frame[4..8] != [0; 4] {
            ensure!(
                commits.len() < crate::antigravity::MAX_DATABASE_STEPS,
                "WAL commit history exceeds the receipt limit"
            );
            let digest = hash.clone().finalize();
            commits.push(uuid::Uuid::from_slice(&digest[..16])?);
        }
    }
    Ok(commits)
}

fn verify_older_headers(
    wal: &mut File,
    frames: u64,
    page_size: u32,
    current: [[u8; 4]; 2],
    mut previous: [[u8; 4]; 2],
) -> Result<()> {
    // The snapshot byte limit bounds the scan. Buffer nearby frame headers for small pages.
    let mut wal = BufReader::with_capacity(64 * 1024, wal);
    let age = |salts: [[u8; 4]; 2]| -> Result<u32> {
        // SQLite increments the first salt and randomizes the second when it resets a WAL.
        let age = u32::from_be_bytes(current[0]).wrapping_sub(u32::from_be_bytes(salts[0]));
        ensure!(
            i32::try_from(age).is_ok_and(|distance| distance > 0) && salts[1] != current[1],
            "ambiguous SQLite WAL generation"
        );
        Ok(age)
    };
    let mut previous_age = age(previous)?;
    for _ in 1..frames {
        let mut header = [0; 24];
        wal.read_exact(&mut header)?;
        let fields = header.as_chunks::<4>().0;
        ensure!(fields[0] != [0; 4], "invalid SQLite WAL page number");
        let salts = [fields[2], fields[3]];
        if salts != previous {
            let next_age = age(salts)?;
            // Each reset overwrites a prefix. A generation deeper in the suffix must be older.
            ensure!(
                next_age > previous_age,
                "interleaved SQLite WAL generations"
            );
            previous = salts;
            previous_age = next_age;
        }
        wal.seek_relative(i64::from(page_size))?;
    }
    Ok(())
}

// See https://www.sqlite.org/fileformat2.html#wal_file_format.
fn checksum(bytes: &[u8], read: fn([u8; 4]) -> u32, mut sum: [u32; 2]) -> [u32; 2] {
    for &[a, b] in bytes.as_chunks::<4>().0.as_chunks::<2>().0 {
        sum[0] = sum[0].wrapping_add(read(a)).wrapping_add(sum[1]);
        sum[1] = sum[1].wrapping_add(read(b)).wrapping_add(sum[0]);
    }
    sum
}
