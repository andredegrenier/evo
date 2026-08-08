//! Blobs in an S3 bucket, for a library that outlives the disk it is on.
//!
//! Only the blobs. redb, tantivy, the page cache and the thumbnails stay on
//! local disk whatever this is set to: they are memory-mapped files and
//! directories of small writes, and neither is a thing object storage does.
//! What goes to S3 is exactly what is content-addressed and never rewritten --
//! the PDFs themselves, at `docs/<sha256>.pdf`.
//!
//! The awkward part is that [`BlobStore`] is synchronous, called from the
//! indexer thread, from `spawn_blocking` tasks and from the desktop app's UI
//! thread, and `object_store` is async. The answer is [`McpClients::block`]'s:
//! a small runtime of this store's own, work posted to it, and the caller waits
//! on a channel. Not `Handle::block_on`, which panics on a thread that is
//! already inside a runtime -- which most of these callers are.
//!
//! Credentials are never in evo's configuration. `AmazonS3Builder::from_env`
//! reads the ordinary `AWS_*` variables and, on an EC2 instance with no
//! variables set, the instance role -- so a server is given access by being the
//! machine it is, and evo has no secret to leak.
//!
//! [`McpClients::block`]: crate::mcp::client::McpClients

use std::io;
use std::sync::Arc;
use std::time::Duration;

use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::BlobStore;

/// How long any one request to the bucket may take.
///
/// Generous: a scanned book is tens of megabytes and the machine may be on the
/// other side of a slow link. The point of the limit is that a request which
/// will never answer does not hold an indexer thread for ever.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the documents sit in the bucket. Under a prefix rather than at the
/// root so a bucket can hold something else as well.
pub const PREFIX: &str = "docs";

/// A [`BlobStore`] backed by an S3 bucket.
pub struct S3BlobStore {
    store: Arc<dyn ObjectStore>,
    /// `Option` only so [`Drop`] can take it: see the comment there.
    runtime: Option<tokio::runtime::Runtime>,
    /// Everything before the id: `docs`, or `<prefix>/docs`.
    root: String,
}

impl S3BlobStore {
    /// The bucket named in the configuration, with credentials from the
    /// environment.
    ///
    /// Nothing is fetched here, so a bucket that does not exist is not
    /// discovered until the first document is read -- the builder only checks
    /// that it has been told enough to try.
    pub fn from_env(bucket: &str, prefix: Option<&str>) -> Result<Self, String> {
        let store = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .map_err(|e| {
                format!(
                    "evo could not set up the S3 bucket \u{201c}{bucket}\u{201d}: {e}. The \
                     region and credentials come from the environment (AWS_REGION, \
                     AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, or an instance role)."
                )
            })?;
        Self::new(Arc::new(store), prefix)
    }

    /// Any object store at all, which is how the tests use an in-memory one.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: Option<&str>) -> Result<Self, String> {
        // Two workers: one request at a time is the ordinary case, and the
        // second is there so a large upload does not stall a small read.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("evo-s3")
            .enable_all()
            .build()
            .map_err(|e| format!("evo could not start the S3 client: {e}"))?;
        let root = match prefix.map(str::trim).filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{}/{PREFIX}", prefix.trim_matches('/')),
            None => PREFIX.to_owned(),
        };
        Ok(Self {
            store,
            runtime: Some(runtime),
            root,
        })
    }

    /// Where one document lives. The id is a sha256 and has been checked to be
    /// one everywhere it comes from, so this cannot be made to name anything
    /// outside the prefix.
    fn key(&self, id: &str) -> Path {
        Path::from(format!("{}/{id}.pdf", self.root))
    }

    /// Run one request on the store's own runtime and wait for it here.
    ///
    /// `Handle::block_on` would panic when called from a thread that is already
    /// inside a runtime, and most of the callers of a `BlobStore` are; posting
    /// the work and waiting on a channel is correct from any thread.
    fn block<T: Send + 'static>(
        &self,
        work: impl Future<Output = object_store::Result<T>> + Send + 'static,
    ) -> io::Result<T> {
        let Some(runtime) = &self.runtime else {
            return Err(io::Error::other("the S3 client has been shut down"));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let _ = tx.send(work.await);
        });
        match rx.recv_timeout(CALL_TIMEOUT) {
            // object_store's own mapping, which is the one worth having: a
            // missing object becomes `NotFound`, and everything above this
            // treats that as "no such document" rather than as a failure.
            Ok(result) => result.map_err(io::Error::from),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the S3 bucket did not answer within {} seconds",
                    CALL_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    /// Whether the bucket already holds this object.
    fn holds(&self, path: &Path) -> io::Result<bool> {
        let store = self.store.clone();
        let path = path.clone();
        match self.block(async move { store.head(&path).await }) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

impl Drop for S3BlobStore {
    fn drop(&mut self) {
        // Dropping a runtime waits for its threads, and waiting is not allowed
        // on every thread this might be dropped from -- `evo serve` lets go of
        // its library from inside another runtime. Handing the shutdown to the
        // runtime's own threads is the version that cannot panic.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl BlobStore for S3BlobStore {
    fn put(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.key(id);
        // Content-addressed: the same id is the same bytes for ever, so an
        // object that is already there is the object being written. A HEAD is
        // a great deal cheaper than uploading a book again.
        if self.holds(&path)? {
            return Ok(());
        }
        let payload = PutPayload::from(bytes.to_vec());
        let store = self.store.clone();
        self.block(async move { store.put(&path, payload).await })?;
        Ok(())
    }

    fn get(&self, id: &str) -> io::Result<Vec<u8>> {
        let path = self.key(id);
        let store = self.store.clone();
        let bytes = self.block(async move { store.get(&path).await?.bytes().await })?;
        Ok(bytes.to_vec())
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        let path = self.key(id);
        let store = self.store.clone();
        match self.block(async move { store.delete(&path).await }) {
            // Deleting what is not there is what was wanted, the same as it is
            // on disk: the local store says so too.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    /// The real interface, over a store that is a `HashMap`. Everything about
    /// this file except which bucket the bytes land in is exercised here: the
    /// keys, the runtime, the waiting, and the error mapping.
    fn store() -> S3BlobStore {
        S3BlobStore::new(Arc::new(InMemory::new()), None).expect("an in-memory store")
    }

    fn id(byte: u8) -> String {
        crate::library::hex_digest(&[byte])
    }

    #[test]
    fn a_document_goes_up_comes_back_and_can_be_taken_away() {
        let blobs = store();
        let id = id(1);
        let bytes = b"%PDF-1.7 not really a pdf".to_vec();

        blobs.put(&id, &bytes).expect("uploaded");
        assert_eq!(blobs.get(&id).expect("downloaded"), bytes);

        blobs.delete(&id).expect("deleted");
        let gone = blobs.get(&id).expect_err("it was deleted");
        assert_eq!(gone.kind(), io::ErrorKind::NotFound);
    }

    /// The id is a digest of the bytes, so writing the same document twice is
    /// writing the same bytes twice. The second one is skipped rather than
    /// re-uploaded -- which is also what makes an interrupted import safe to
    /// run again.
    #[test]
    fn writing_the_same_document_twice_does_not_upload_it_twice() {
        let blobs = store();
        let id = id(2);
        blobs.put(&id, b"the first copy").expect("uploaded");
        // Different bytes under the same id cannot happen in evo -- the id is
        // the digest -- and if it is asked for, what is already there wins.
        blobs.put(&id, b"a different thing").expect("skipped");
        assert_eq!(blobs.get(&id).expect("downloaded"), b"the first copy");
    }

    /// The one error the callers tell apart: "there is no such document" has to
    /// arrive as `NotFound` and not as a generic failure, because the library
    /// reads it as an empty answer rather than as something being wrong.
    #[test]
    fn a_document_that_was_never_there_is_not_found() {
        let blobs = store();
        let missing = blobs.get(&id(3)).expect_err("never uploaded");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(
            missing.to_string().to_lowercase().contains("not found"),
            "{missing}"
        );
        // And deleting what is not there is not a failure, exactly as on disk.
        blobs.delete(&id(3)).expect("nothing to delete");
    }

    /// Two documents are two objects, under keys that say what they are.
    #[test]
    fn every_document_is_one_object_under_the_documents_prefix() {
        let blobs = store();
        assert_eq!(blobs.key(&id(4)).as_ref(), format!("docs/{}.pdf", id(4)));

        let prefixed = S3BlobStore::new(Arc::new(InMemory::new()), Some("/evo/")).expect("a store");
        assert_eq!(
            prefixed.key(&id(4)).as_ref(),
            format!("evo/docs/{}.pdf", id(4)),
            "a prefix goes in front, and the slashes are not doubled"
        );
        assert_eq!(
            S3BlobStore::new(Arc::new(InMemory::new()), Some("  "))
                .expect("a store")
                .key(&id(4))
                .as_ref(),
            format!("docs/{}.pdf", id(4)),
            "a blank prefix is no prefix"
        );

        blobs.put(&id(5), b"one").expect("uploaded");
        blobs.put(&id(6), b"two").expect("uploaded");
        assert_eq!(blobs.get(&id(5)).unwrap(), b"one");
        assert_eq!(blobs.get(&id(6)).unwrap(), b"two");
    }

    /// A whole library over object storage: the blobs are in the bucket and
    /// everything else -- redb, the sidecars -- is on disk, which is the split
    /// this backend exists to make.
    #[test]
    fn a_library_can_keep_its_documents_in_a_bucket() {
        let dir = std::env::temp_dir().join(format!("evo-s3-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = Arc::new(store());
        let library = crate::library::Library::open_at_with_blobs(dir.clone(), blobs.clone())
            .expect("a library");

        let meta = library
            .import(std::path::Path::new("tests/fixtures/sample.pdf"))
            .expect("imported");
        assert_eq!(meta.page_count, 2);
        // Nothing landed in the local blob directory, and the bucket has it.
        assert!(
            !dir.join("docs").join(format!("{}.pdf", meta.id)).exists(),
            "the PDF was written to disk as well"
        );
        assert_eq!(
            crate::library::hex_digest(&library.load_bytes(&meta.id).expect("read back")),
            meta.id
        );

        library.delete(&meta.id).expect("deleted");
        assert_eq!(
            blobs
                .get(&meta.id)
                .expect_err("gone from the bucket")
                .kind(),
            io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real thing, against a real bucket, which needs credentials and costs
    /// a fraction of a cent -- so it is opt-in:
    ///
    /// ```text
    /// EVO_S3_TEST_BUCKET=my-bucket AWS_REGION=eu-west-2 \
    ///   cargo test --features s3 -- --ignored real_bucket
    /// ```
    #[test]
    #[ignore = "writes to a real S3 bucket"]
    fn a_real_bucket_takes_a_document_and_gives_it_back() {
        let Ok(bucket) = std::env::var("EVO_S3_TEST_BUCKET") else {
            eprintln!("set EVO_S3_TEST_BUCKET to run this");
            return;
        };
        let blobs =
            S3BlobStore::from_env(&bucket, std::env::var("EVO_S3_TEST_PREFIX").ok().as_deref())
                .expect("the bucket");

        // A document nobody else's test would write: the id is the digest of
        // bytes with this run's own time in them.
        let bytes = format!("evo test {:?}", std::time::SystemTime::now()).into_bytes();
        let id = crate::library::hex_digest(&bytes);
        assert_eq!(
            blobs.get(&id).expect_err("a fresh id").kind(),
            io::ErrorKind::NotFound
        );

        blobs.put(&id, &bytes).expect("uploaded");
        assert_eq!(blobs.get(&id).expect("downloaded"), bytes);
        blobs.put(&id, &bytes).expect("the second put is a HEAD");

        blobs.delete(&id).expect("deleted");
        assert_eq!(
            blobs.get(&id).expect_err("deleted").kind(),
            io::ErrorKind::NotFound
        );
    }
}
