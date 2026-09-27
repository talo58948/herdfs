//! Ring R3, U3: DuckDB writes a ~512 MiB Parquet file of 40 columns on `maker`; a DuckDB query of
//! 3 columns on `reader`, over a WAN link with a cold cache, fetches little more than the chunks
//! under those columns and the footer.

use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy, overlapping_chunk_bytes, run};
use serde::Deserialize;

const TABLE: &str = "table.parquet";
const COLUMNS: usize = 40;
/// Random 8-byte values in 40 columns: about 512 MiB in 14 row groups of 122,880 rows.
const ROWS: u64 = 1_677_722;
const ROW_GROUP: u64 = 122_880;
/// Spread over the row, so their column chunks are not neighbours.
const SELECTED: [&str; 3] = ["c3", "c17", "c31"];
/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);

/// One column chunk as `parquet_metadata()` places it.
#[derive(Deserialize)]
struct ColumnChunk {
    start: u64,
    len: u64,
}

#[test]
fn a_query_of_three_columns_fetches_about_their_chunks() -> Result<()> {
    // contract: C9
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(());
    }
    let lab = Lab::start(
        LabConfig::default()
            .client("maker", "alice", &[], Profile::Lan, Strategy::Stream)
            .client("reader", "bob", &[], Profile::Wan, Strategy::Stream),
    )?;
    let columns: Vec<String> = (0..COLUMNS).map(|k| format!("hash(i, {k}) AS c{k}")).collect();
    let rows = format!("SELECT {} FROM range({ROWS}) AS t(i)", columns.join(", "));
    let table = lab.mount_path("maker").join(TABLE);
    let started = Instant::now();
    duckdb(&format!(
        "COPY ({rows}) TO '{}' (FORMAT parquet, ROW_GROUP_SIZE {ROW_GROUP})",
        table.display()
    ))?;
    // fsync returns once the authority has committed the version.
    File::open(&table)?.sync_all()?;
    let made = started.elapsed();
    let version = lab.head("alice", TABLE)?;
    let ranges = selected_ranges(&table)?;
    thread::sleep(VISIBLE_AFTER);

    let sums = |source: &str| {
        let sums = SELECTED.map(|column| format!("sum({column})::VARCHAR AS {column}"));
        duckdb(&format!("SELECT {} FROM {source}", sums.join(", ")))
    };
    let started = Instant::now();
    let answer = sums(&format!("'{}'", lab.mount_path("reader").join(TABLE).display()))?;
    let queried = started.elapsed();
    assert_eq!(
        answer,
        sums(&format!("({rows})"))?,
        "the query differs from its source rows"
    );

    let fetched = lab.stats("reader")?["bytes_fetched"]
        .as_u64()
        .context("no bytes_fetched")?;
    let needed = overlapping_chunk_bytes(&version.chunks, &ranges);
    assert!(
        fetched * 5 <= needed * 6,
        "fetched {fetched} B; the column chunks and footer lie in {needed} B of chunks"
    );
    let selected: u64 = ranges.iter().map(|range| range.end - range.start).sum();
    eprintln!(
        "U3 columnar: {} B parquet in {} chunks, made in {made:.1?}; {} column chunks and footer = \
         {selected} B, in {needed} B of chunks; fetched {fetched} B ({:.2}x) in {queried:.2?}",
        version.size,
        version.chunks.len(),
        ranges.len() - 1,
        fetched as f64 / needed as f64
    );
    Ok(())
}

/// Runs `sql` in a fresh in-memory DuckDB and returns its result as JSON.
fn duckdb(sql: &str) -> Result<String> {
    run(Command::new("duckdb").args(["-json", "-c", &format!("SET threads = 4; {sql}")]))
}

/// The byte ranges of the selected columns' chunks in every row group, and of the footer.
fn selected_ranges(table: &Path) -> Result<Vec<Range<u64>>> {
    let names = SELECTED.map(|column| format!("'{column}'")).join(", ");
    let chunks = duckdb(&format!(
        "SELECT coalesce(dictionary_page_offset, data_page_offset) AS start, \
         total_compressed_size AS len FROM parquet_metadata('{}') WHERE path_in_schema IN ({names})",
        table.display()
    ))?;
    let chunks: Vec<ColumnChunk> = serde_json::from_str(&chunks)?;
    ensure!(chunks.len() > SELECTED.len(), "expected several row groups");
    let mut ranges: Vec<_> = (chunks.iter())
        .map(|chunk| chunk.start..chunk.start + chunk.len)
        .collect();
    ranges.push(footer(table)?);
    Ok(ranges)
}

/// A Parquet file ends with its footer, the footer's length as 4 little-endian bytes, and `PAR1`.
fn footer(table: &Path) -> Result<Range<u64>> {
    let file = File::open(table)?;
    let size = file.metadata()?.len();
    let mut tail = [0; 8];
    file.read_exact_at(&mut tail, size - 8)?;
    let (len, magic) = tail.split_at(4);
    ensure!(magic == b"PAR1", "{} does not end like Parquet", table.display());
    let len = u32::from_le_bytes(len.try_into()?);
    Ok(size - 8 - u64::from(len)..size)
}
