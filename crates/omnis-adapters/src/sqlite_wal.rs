//! Checks an empty result against the WAL in an already verified private database copy.

use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Result, ensure};
use rusqlite::Connection;

/// The private SQLite snapshot cannot confirm that an empty result covers the native WAL.
#[derive(Debug)]
pub struct UnverifiedSqliteWal(pub(crate) String);

impl fmt::Display for UnverifiedSqliteWal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQLite WAL cannot confirm that the session is empty ({})",
            self.0
        )
    }
}

impl std::error::Error for UnverifiedSqliteWal {}

const HEADER_BYTES: u64 = 32;
const FRAME_HEADER_BYTES: u64 = 24;
// A large write still in progress can wait for its next commit before an empty result is published.
const MAX_UNCOMMITTED_BYTES: u64 = 4 * 1024 * 1024;

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
    let mut work = 0;
    for _ in 0..(len - prefix_end) / stride {
        let mut frame = [0; 24];
        wal.read_exact(&mut frame)?;
        let fields = frame.as_chunks::<4>().0;
        let older_generation = fields[2..4] != words[4..6];
        if older_generation {
            // A reset increments the first salt and randomizes the second. Old frames may remain
            // after a shorter transaction overwrites the beginning of a previous WAL.
            let age = u32::from_be_bytes(words[4]).wrapping_sub(u32::from_be_bytes(fields[2]));
            ensure!(
                i32::try_from(age).is_ok_and(|distance| distance > 0) && fields[3] != words[5],
                "ambiguous SQLite WAL generation"
            );
        } else {
            ensure!(
                fields[0] != [0; 4] && fields[1] == [0; 4],
                "unverified SQLite WAL commit"
            );
            work += stride;
            ensure!(
                work <= MAX_UNCOMMITTED_BYTES,
                "uncommitted SQLite WAL exceeds verification budget"
            );
        }
        wal.read_exact(&mut page)?;
        running = checksum(&frame[..8], read, running);
        running = checksum(&page, read, running);
        let matches =
            running[0].to_be_bytes() == fields[4] && running[1].to_be_bytes() == fields[5];
        if older_generation {
            // Frame checksums exclude the salts. A current checksum makes the apparent reset ambiguous.
            ensure!(
                !matches,
                "SQLite WAL frame has conflicting generation and checksum"
            );
            return Ok(());
        }
        ensure!(matches, "unverified SQLite WAL frame checksum");
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
