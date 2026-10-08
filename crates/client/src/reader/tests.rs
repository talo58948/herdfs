use std::future::ready;
use std::time::Duration;

use futures::FutureExt;

use herdfs_core::ChunkRef;
use herdfs_core::proto::{EntryId, Version};

use super::*;

/// A reader over a fresh cache, and a version whose chunk `i` is 100 + i bytes of `i`.
fn setup(dir: &std::path::Path) -> (Reader, OpenVersion, Vec<Vec<u8>>) {
    let token = dir.join("token");
    std::fs::write(&token, "t").unwrap();
    let remote = Remote::new("http://127.0.0.1:9", "test", token, &[]).unwrap();
    let cache = Cache::open(dir.join("cache"), 1 << 20).unwrap();
    let reader = Reader::new(Arc::new(remote), Arc::new(cache), Arc::default());
    let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100 + usize::from(i)]).collect();
    let refs = chunks.iter().map(|bytes| ChunkRef {
        id: ChunkId::of(bytes),
        len: bytes.len() as u32,
    });
    let version = Version {
        id: VersionId(1),
        entry: EntryId(1),
        size: chunks.iter().map(|chunk| chunk.len() as u64).sum(),
        author: "user:test".into(),
        created_ms: 0,
        chunks: refs.collect(),
    };
    (reader, OpenVersion::new(version), chunks)
}

async fn read(
    reader: &Reader,
    file: &OpenVersion,
    chunks: &[Vec<u8>],
    (off, len): (u64, u64),
) -> (Vec<u8>, Vec<usize>) {
    let mut fetched = Vec::new();
    let fetch = |missing: Vec<usize>| {
        fetched.extend(&missing);
        let bytes: Vec<_> = missing.iter().map(|&index| Ok(chunks[index].clone())).collect();
        ready(Ok(stream::iter(bytes).boxed()))
    };
    let bytes = reader.read_with(file, off, len, fetch).await.unwrap();
    (bytes, fetched)
}

// contract: C9
#[tokio::test]
async fn a_cold_read_fetches_only_the_overlapping_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    let whole = chunks.concat();

    // Chunk ends: 100, 201, 303, 406, 510.
    let (bytes, fetched) = read(&reader, &file, &chunks, (150, 50)).await;
    assert_eq!(bytes, whole[150..200]);
    assert_eq!(fetched, [1]);

    let (bytes, fetched) = read(&reader, &file, &chunks, (199, 201)).await;
    assert_eq!(bytes, whole[199..400]);
    assert_eq!(fetched, [2, 3]);
}

// contract: C8, C9
#[tokio::test]
async fn a_warm_read_is_served_from_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    read(&reader, &file, &chunks, (0, 1_000)).await;

    let (bytes, fetched) = read(&reader, &file, &chunks, (0, 1_000)).await;
    assert_eq!(bytes, chunks.concat());
    assert!(fetched.is_empty());
}

// contract: C9
#[tokio::test]
async fn verified_chunks_are_served_from_memory() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    read(&reader, &file, &chunks, (0, 1_000)).await;
    std::fs::remove_dir_all(dir.path().join("cache")).unwrap();

    let (bytes, fetched) = read(&reader, &file, &chunks, (0, 1_000)).await;
    assert_eq!(bytes, chunks.concat());
    assert!(fetched.is_empty());
}

// contract: C11
#[tokio::test]
async fn a_tampered_cached_chunk_is_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    read(&reader, &file, &chunks, (0, 1_000)).await;
    std::fs::write(dir.path().join("cache").join(file.id(3).to_string()), b"evil").unwrap();
    // The first reader keeps verified chunks in memory; a fresh one loads them from disk.
    let (reader, ..) = setup(dir.path());

    let (bytes, fetched) = read(&reader, &file, &chunks, (0, 1_000)).await;
    assert_eq!(bytes, chunks.concat());
    assert_eq!(fetched, [3]);
}

// contract: C9
#[tokio::test]
async fn concurrent_reads_fetch_a_chunk_once() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    let fetched = std::sync::Mutex::new(Vec::<usize>::new());
    let fetch = |missing: Vec<usize>| {
        fetched.lock().unwrap().extend(&missing);
        let bytes: Vec<_> = missing.iter().map(|&index| Ok(chunks[index].clone())).collect();
        async move {
            tokio::task::yield_now().await;
            Ok(stream::iter(bytes).boxed())
        }
    };
    let (first, second) = tokio::join!(
        reader.read_with(&file, 0, 50, fetch),
        reader.read_with(&file, 20, 50, fetch)
    );
    assert_eq!(first.unwrap(), chunks[0][..50]);
    assert_eq!(second.unwrap(), chunks[0][20..70]);
    assert_eq!(*fetched.lock().unwrap(), [0]);
}

// contract: C9
#[tokio::test]
async fn overlapping_prefetch_batches_fetch_each_chunk_once() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    let fetched = std::sync::Mutex::new(Vec::<usize>::new());
    let fetch = |missing: Vec<usize>| {
        fetched.lock().unwrap().extend(&missing);
        let bytes: Vec<_> = missing.iter().map(|&index| Ok(chunks[index].clone())).collect();
        async move {
            tokio::task::yield_now().await;
            Ok(stream::iter(bytes).boxed())
        }
    };
    let (first, second) = tokio::join!(
        reader.load(&file, &[0, 1, 2], fetch),
        reader.load(&file, &[1, 2, 3], fetch)
    );
    assert_eq!(first.unwrap().len(), 3);
    assert_eq!(second.unwrap().len(), 3);
    let mut fetched = fetched.into_inner().unwrap();
    fetched.sort_unstable();
    assert_eq!(fetched, [0, 1, 2, 3]);
}

// contract: C9
#[tokio::test]
async fn a_read_waits_only_for_its_own_chunk_of_a_prefetch_batch() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, file, chunks) = setup(dir.path());
    let rest_sent = tokio::sync::Notify::new();
    // A batch whose first chunk arrives at once, and the rest only after the read is done.
    let batch = |missing: Vec<usize>| {
        let mut bytes = missing.into_iter().map(|index| Ok(chunks[index].clone()));
        let first = stream::iter(bytes.next());
        let rest = rest_sent
            .notified()
            .map(|()| stream::iter(bytes.collect::<Vec<_>>()));
        ready(Ok(first.chain(rest.flatten_stream()).boxed()))
    };
    let read_first = async {
        // Lets the batch claim its chunks first, as a prefetch started at open does.
        tokio::task::yield_now().await;
        let read = read(&reader, &file, &chunks, (0, 50)).await;
        rest_sent.notify_one();
        read
    };
    let both = async { tokio::join!(read_first, reader.load(&file, &[0, 1, 2], batch)) };
    let ((bytes, fetched), batch) = tokio::time::timeout(Duration::from_secs(5), both)
        .await
        .expect("the read waits for the whole batch");
    assert_eq!((bytes, fetched), (chunks[0][..50].to_vec(), vec![]));
    assert_eq!(batch.unwrap().len(), 3);
}
