//! Integration tests for the `epoch-cas` kernel, covering the scenarios the
//! crate spec calls out:
//!  1. put/get round-trip over `SyncGenericCAS<MemoryByteBackend, IdentityCodec>`
//!  2. content-addressing + dedup
//!  3. integrity verification on corrupt entries
//!  4. has / delete
//!  5. a non-identity codec, and a pluggable non-default ContentHasher
//!  6. Blocking / Asyncify sync↔async bridges
//!  7. BatchingCAS write→flush→read-back

use std::sync::Arc;

use epoch_cas::{
    AsyncCAS, AsyncGenericCAS, AsyncMemoryByteBackend, Asyncify, BatchingCAS, Blake3Hasher,
    Blocking, Bytes, CASError, CASHash, Codec, ContentHasher, IdentityCodec, MemoryByteBackend,
    SyncByteBackend, SyncCAS, SyncGenericCAS,
};

type IdentitySyncCas = SyncGenericCAS<MemoryByteBackend, IdentityCodec>;

fn new_identity_cas() -> (MemoryByteBackend, IdentitySyncCas) {
    let backend = MemoryByteBackend::new();
    let cas = SyncGenericCAS::<_, IdentityCodec>::new(backend.clone());
    (backend, cas)
}

// 1. put/get round-trip.
#[test]
fn put_get_roundtrip() {
    let (_backend, cas) = new_identity_cas();
    let payload = Bytes::from_static(b"hello, content-addressed world");
    let id = cas.put(payload.clone()).unwrap();
    let got = cas.get(&id).unwrap();
    assert_eq!(got, Some(payload));
}

#[test]
fn get_missing_returns_none() {
    let (_backend, cas) = new_identity_cas();
    let absent = Blake3Hasher::hash(b"never stored");
    assert_eq!(cas.get(&absent).unwrap(), None);
}

#[test]
fn id_is_hash_of_uncompressed_encoding() {
    let (_backend, cas) = new_identity_cas();
    let payload = Bytes::from_static(b"determinism");
    let id = cas.put(payload.clone()).unwrap();
    // IdentityCodec means the canonical encoding is the payload itself.
    assert_eq!(id, Blake3Hasher::hash(&payload));
}

// 2. content-addressing + dedup: identical content → identical id, single entry.
#[test]
fn dedup_identical_content() {
    let (backend, cas) = new_identity_cas();
    let payload = Bytes::from_static(b"the same bytes twice");
    let id1 = cas.put(payload.clone()).unwrap();
    let id2 = cas.put(payload.clone()).unwrap();
    assert_eq!(id1, id2);
    // Two puts of identical content leave exactly one backend entry.
    assert_eq!(backend.len(), 1);
}

#[test]
fn distinct_content_distinct_ids() {
    let (backend, cas) = new_identity_cas();
    let a = cas.put(Bytes::from_static(b"alpha")).unwrap();
    let b = cas.put(Bytes::from_static(b"beta")).unwrap();
    assert_ne!(a, b);
    assert_eq!(backend.len(), 2);
}

// 3. integrity: corrupt a backend entry → get returns IntegrityMismatch.
#[test]
fn corrupt_entry_yields_integrity_mismatch() {
    let (backend, cas) = new_identity_cas();
    let id = cas.put(Bytes::from_static(b"trustworthy")).unwrap();

    // Overwrite the stored (compressed) bytes with a *valid zstd stream of
    // different content*, so decompression succeeds but the re-hash differs.
    let evil = epoch_cas::compress(b"tampered payload").unwrap();
    backend.corrupt(&id, evil);

    let err = cas.get(&id).unwrap_err();
    match err {
        CASError::IntegrityMismatch { expected, actual } => {
            assert_eq!(expected, id);
            assert_ne!(actual, id);
            assert_eq!(actual, Blake3Hasher::hash(b"tampered payload"));
        }
        other => panic!("expected IntegrityMismatch, got {other:?}"),
    }
}

// 4. has / delete.
#[test]
fn has_and_delete() {
    let (_backend, cas) = new_identity_cas();
    let id_a = cas.put(Bytes::from_static(b"a")).unwrap();
    let id_b = cas.put(Bytes::from_static(b"b")).unwrap();

    assert!(cas.has(&id_a).unwrap());
    assert!(cas.has(&id_b).unwrap());
    assert!(!cas.has(&Blake3Hasher::hash(b"absent")).unwrap());

    cas.delete(&id_a).unwrap();
    assert!(!cas.has(&id_a).unwrap());
    assert!(cas.has(&id_b).unwrap());
    // delete of an absent id is a no-op, not an error.
    cas.delete(&id_a).unwrap();
}

// 5. a non-identity Codec<V> over a small struct.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Point {
    x: i32,
    y: i32,
}

/// Fixed-width little-endian encoding of `Point` (8 bytes).
struct PointCodec;

impl Codec<Point> for PointCodec {
    fn encode(value: &Point) -> Bytes {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(&value.x.to_le_bytes());
        buf.extend_from_slice(&value.y.to_le_bytes());
        Bytes::from(buf)
    }

    fn decode(bytes: &[u8]) -> Result<Point, CASError> {
        if bytes.len() != 8 {
            return Err(CASError::Codec(format!(
                "Point needs 8 bytes, got {}",
                bytes.len()
            )));
        }
        let x = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let y = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
        Ok(Point { x, y })
    }
}

#[test]
fn non_identity_codec_roundtrip_and_dedup() {
    let backend = MemoryByteBackend::new();
    let cas = SyncGenericCAS::<_, PointCodec>::new(backend.clone());

    let p = Point { x: -7, y: 42 };
    let id = cas.put(p.clone()).unwrap();
    assert_eq!(cas.get(&id).unwrap(), Some(p.clone()));

    // Structural dedup works through the codec: same struct → same id.
    let id2 = cas.put(p).unwrap();
    assert_eq!(id, id2);
    assert_eq!(backend.len(), 1);

    // A different struct addresses differently.
    let other = cas.put(Point { x: 7, y: 42 }).unwrap();
    assert_ne!(id, other);
}

#[test]
fn non_identity_codec_decode_error_surfaces() {
    // Manually store garbage of the wrong length under its own hash, so the
    // integrity check passes but decode fails.
    let backend = MemoryByteBackend::new();
    let cas = SyncGenericCAS::<_, PointCodec>::new(backend.clone());

    let bad = Bytes::from_static(b"xyz"); // 3 bytes, not 8
    let id = Blake3Hasher::hash(&bad);
    backend.put(&id, epoch_cas::compress(&bad).unwrap()).unwrap();

    match cas.get(&id).unwrap_err() {
        CASError::Codec(_) => {}
        other => panic!("expected Codec error, got {other:?}"),
    }
}

// 5b. a pluggable, non-default ContentHasher threaded through the engine.
//
// `DoubleBlake3` hashes the hash: deterministic, still 32 bytes, but distinct
// from a single BLAKE3 pass — so we can prove the CAS actually addresses with
// the `H` it was given (both on write and on the read-side integrity check).
struct DoubleBlake3;

impl ContentHasher for DoubleBlake3 {
    fn hash(bytes: &[u8]) -> CASHash {
        let once = Blake3Hasher::hash(bytes);
        Blake3Hasher::hash(once.as_bytes())
    }
}

#[test]
fn custom_hasher_addresses_with_its_own_digest() {
    let backend = MemoryByteBackend::new();
    let cas = SyncGenericCAS::<_, IdentityCodec, DoubleBlake3>::new(backend.clone());

    let payload = Bytes::from_static(b"hashed by the plugged-in hasher");
    let id = cas.put(payload.clone()).unwrap();

    // The id is the custom digest, not the default BLAKE3 one.
    assert_eq!(id, DoubleBlake3::hash(&payload));
    assert_ne!(id, Blake3Hasher::hash(&payload));

    // Round-trip works: the read-side integrity check re-hashes with the same H.
    assert_eq!(cas.get(&id).unwrap(), Some(payload));
}

// 6. Blocking and Asyncify bridge round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asyncify_bridge_roundtrip() {
    // Wrap a sync CAS as an async one.
    let backend = MemoryByteBackend::new();
    let sync_cas = Arc::new(SyncGenericCAS::<_, IdentityCodec>::new(backend.clone()));
    let async_cas = Asyncify::new(sync_cas);

    let payload = Bytes::from_static(b"via spawn_blocking");
    let id = async_cas.put(payload.clone()).await.unwrap();
    assert!(async_cas.has(&id).await.unwrap());
    assert_eq!(async_cas.get(&id).await.unwrap(), Some(payload));

    async_cas.delete(&id).await.unwrap();
    assert!(!async_cas.has(&id).await.unwrap());
}

#[test]
fn blocking_bridge_roundtrip() {
    // Build an async CAS, then drive it synchronously via a held runtime handle.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let async_backend = AsyncMemoryByteBackend::new();
    let async_cas = AsyncGenericCAS::<_, IdentityCodec>::new(async_backend);
    // `Blocking` must run on a non-runtime thread; the test thread qualifies.
    let sync_view = Blocking::new(async_cas, rt.handle().clone());

    let payload = Bytes::from_static(b"driven by block_on");
    let id = sync_view.put(payload.clone()).unwrap();
    assert!(sync_view.has(&id).unwrap());
    assert_eq!(sync_view.get(&id).unwrap(), Some(payload));

    sync_view.delete(&id).unwrap();
    assert!(!sync_view.has(&id).unwrap());
}

// 7. BatchingCAS write→flush→read-back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batching_write_flush_readback() {
    let async_backend = AsyncMemoryByteBackend::new();
    let inner = Arc::new(AsyncGenericCAS::<_, IdentityCodec>::new(async_backend.clone()));
    let batching = BatchingCAS::<_, Bytes, IdentityCodec>::new(Arc::clone(&inner), 16);

    let payload = Bytes::from_static(b"coalesced write");
    // put returns the content-address immediately, before persistence.
    let id = batching.put(payload.clone()).await.unwrap();
    assert_eq!(id, Blake3Hasher::hash(&payload));

    // Drain the queue, then the value is durable and readable back.
    batching.flush().await.unwrap();
    assert_eq!(async_backend.len(), 1);
    assert_eq!(batching.get(&id).await.unwrap(), Some(payload.clone()));
    // It's the same backend the inner store sees.
    assert_eq!(inner.get(&id).await.unwrap(), Some(payload));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batching_many_writes_all_persist() {
    let async_backend = AsyncMemoryByteBackend::new();
    let inner = Arc::new(AsyncGenericCAS::<_, IdentityCodec>::new(async_backend.clone()));
    let batching = BatchingCAS::<_, Bytes, IdentityCodec>::new(Arc::clone(&inner), 4);

    let mut ids = Vec::new();
    for i in 0..50u32 {
        let v = Bytes::from(format!("payload-{i}").into_bytes());
        ids.push(batching.put(v).await.unwrap());
    }
    batching.flush().await.unwrap();

    // 50 distinct payloads → 50 entries, every one readable.
    assert_eq!(async_backend.len(), 50);
    for id in &ids {
        assert!(batching.get(id).await.unwrap().is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batching_put_after_shutdown_is_shutdown_error() {
    let async_backend = AsyncMemoryByteBackend::new();
    let inner = Arc::new(AsyncGenericCAS::<_, IdentityCodec>::new(async_backend));
    let batching = BatchingCAS::<_, Bytes, IdentityCodec>::new(inner, 8);

    batching.put(Bytes::from_static(b"before")).await.unwrap();
    batching.shutdown().await;

    // After shutdown the flusher is gone; further puts surface Shutdown.
    match batching.put(Bytes::from_static(b"after")).await {
        Err(CASError::Shutdown) => {}
        other => panic!("expected Shutdown, got {other:?}"),
    }
}
