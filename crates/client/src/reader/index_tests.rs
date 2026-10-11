use super::mp4::mp4_box;
use super::*;

// contract: P3, C9
#[test]
fn parquet_footer_is_the_index() {
    let metadata = b"metadata";
    let file = [
        b"PAR1data".as_slice(),
        metadata,
        &(metadata.len() as u32).to_le_bytes(),
        b"PAR1",
    ]
    .concat();
    assert_eq!(parquet(file.len() as u64, &file), Some(8..file.len() as u64));
}

// contract: P3, C9
#[test]
fn parquet_does_not_follow_metadata_contents() {
    let file = [b"PAR1data".as_slice(), b"\0", &1u32.to_le_bytes(), b"PAR1"].concat();
    assert_eq!(parquet(file.len() as u64, &file), Some(8..17));
}

// contract: P3, C9
#[test]
fn zip_central_directory_is_the_index() {
    let mut file = b"payload".to_vec();
    let central = 7u32;
    let mut header = [0; ZIP_ENTRY_HEADER as usize];
    header[..4].copy_from_slice(b"PK\x01\x02");
    header[28..30].copy_from_slice(&4u16.to_le_bytes());
    file.extend_from_slice(&header);
    file.extend_from_slice(b"name");
    let len = u32::try_from(header.len() + 4).unwrap();
    file.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    file.extend_from_slice(&len.to_le_bytes());
    file.extend_from_slice(&central.to_le_bytes());
    file.extend_from_slice(&[0, 0]);
    let directory = zip(file.len() as u64, 0, &file).next().unwrap();
    assert_eq!(directory.range, 7..57);
    assert_eq!(directory.entries, 1);
    assert_eq!(zip_entry_len(&file[7..53]), Some(50));
}

// contract: P3, C9
#[test]
fn zip_directory_size_must_match_its_entry_count() {
    let mut file = b"fake".to_vec();
    file.extend_from_slice(b"PK\x05\x06\0\0\0\0\0\0\0\0");
    file.extend_from_slice(&4u32.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&[0, 0]);
    assert_eq!(zip(file.len() as u64, 0, &file).next(), None);
}

// contract: P3, C9
#[test]
fn format_index_limit_excludes_mp4_payloads() {
    let parquet_tail = [((MAX_INDEX_REGION - 7) as u32).to_le_bytes().as_slice(), b"PAR1"].concat();
    assert_eq!(parquet(MAX_INDEX_REGION + 8, &parquet_tail), None);

    let mp4 = [((MAX_INDEX_REGION + 1) as u32).to_be_bytes().as_slice(), b"moov"].concat();
    assert_eq!(mp4_box(0, MAX_INDEX_REGION + 1, &mp4), None);

    let mdat = [((MAX_INDEX_REGION + 1) as u32).to_be_bytes().as_slice(), b"mdat"].concat();
    assert!(mp4_box(0, MAX_INDEX_REGION + 1, &mdat).is_some());
}

// contract: P3, C9
#[test]
fn mp4_walk_finds_moov() {
    let file = [
        12u32.to_be_bytes().as_slice(),
        b"ftyp",
        b"test",
        16u32.to_be_bytes().as_slice(),
        b"moov",
        b"metadata",
    ]
    .concat();
    let (_, first, _) = mp4_box(0, file.len() as u64, &file).unwrap();
    let (kind, second, _) = mp4_box(first.end, file.len() as u64, &file[first.end as usize..]).unwrap();
    assert_eq!((&kind, second), (b"moov", 12..28));
}

// contract: P3, C9
#[test]
fn unexpected_bytes_match_no_format() {
    let file = b"not an indexed format";
    assert_eq!(parquet(file.len() as u64, file), None);
    assert_eq!(zip(file.len() as u64, 0, file).next(), None);
    assert!(!mp4_header(file, file.len() as u64));
}
