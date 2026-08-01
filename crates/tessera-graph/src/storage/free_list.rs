// SPDX-License-Identifier: Apache-2.0

//! Page reuse: taking pages off the free directory and putting them back.
//!
//! The algorithm lives here, once, rather than in each storage backend. Both
//! the in-memory and the file-backed store must agree exactly on when a page is
//! reusable — a divergence would mean a graph behaves differently depending on
//! whether it is persisted, and the in-memory tests would stop being evidence
//! about the real thing.
//!
//! Callers see only [`take_free_page`] and [`release_page`]; the directory
//! pages they manipulate are described in
//! [`crate::storage::codec::free_directory_codec`].

use crate::error::Result;
use crate::storage::backend::{DataFile, PageId};
use crate::storage::codec::free_directory_codec::{self, FreeDirectoryPage};
use crate::storage::meta::{FREE_DIRECTORY_EMPTY, FREE_SPARE_EMPTY, GraphMeta};
use crate::storage::page::{PageBuf, magic};

/// The magic stamp of the file a directory page lives in.
///
/// A directory page occupies a page of the very file it describes, so it must
/// carry that file's stamp or the pool's per-file validation rejects it.
const fn file_magic(file: DataFile) -> [u8; 4] {
    match file {
        DataFile::Nodes => magic::NODES,
        DataFile::Edges => magic::EDGES,
        DataFile::Adjacency => magic::ADJACENCY,
        DataFile::Strings => magic::STRINGS,
        DataFile::Overflow => magic::OVERFLOW,
    }
}

/// Everything the free-list algorithm needs from a storage backend.
///
/// Narrower than the full backend contract on purpose: this module must not be
/// able to allocate (that would recurse) or flush.
pub trait FreeListStore {
    fn read_page_raw(&self, file: DataFile, page_id: PageId) -> Result<PageBuf>;
    fn write_page_raw(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()>;
    fn meta_ref(&self) -> &GraphMeta;
    fn meta_mut_ref(&mut self) -> &mut GraphMeta;
}

/// Takes one reusable page off `file`'s free directory, if any.
///
/// Returns `Ok(None)` when nothing is available, which tells the caller to grow
/// the file instead.
///
/// Entries are taken from the tail of the head directory page — the most
/// recently released id — so a page that was just freed is the first handed
/// back. That ordering is deliberate: the freshly released page is the one most
/// likely to still be resident in the buffer pool, so reusing it avoids a disk
/// read that reusing an older id would incur.
///
/// # Errors
///
/// Propagates read/write failures, and rejects a head that does not point at a
/// well-formed directory page.
pub fn take_free_page<S: FreeListStore + ?Sized>(
    store: &mut S,
    file: DataFile,
) -> Result<Option<PageId>> {
    // The metadata-held page first: it costs no page read at all, and it is
    // the one the common "rewrite the same entity" loop keeps cycling through.
    let spare = store.meta_ref().free_spare_page(file);
    if spare != FREE_SPARE_EMPTY {
        store.meta_mut_ref().set_free_spare_page(file, FREE_SPARE_EMPTY);
        decrement_free_count(store, file);
        return Ok(Some(spare));
    }

    let head = store.meta_ref().free_directory_head(file);
    if head == FREE_DIRECTORY_EMPTY {
        return Ok(None);
    }

    let buf = store.read_page_raw(file, head)?;
    let mut dir = free_directory_codec::decode(&buf, file.file_name(), head)?;

    if let Some(id) = dir.entries.pop() {
        // The directory page keeps its role; only its contents shrink.
        let encoded = free_directory_codec::encode(&dir, file_magic(file))?;
        store.write_page_raw(file, head, &encoded)?;
        decrement_free_count(store, file);
        return Ok(Some(id));
    }

    // An empty head page is itself reusable: hand it out and promote its
    // successor. Without this the directory would accumulate empty pages,
    // leaking the very space it exists to reclaim.
    let next = dir.next;
    store.meta_mut_ref().set_free_directory_head(file, next);
    Ok(Some(head))
}

/// Returns `page_id` to `file`'s free directory.
///
/// # Errors
///
/// Propagates read/write failures. Note that the *first* release on a file with
/// no directory yet consumes the released page itself to hold the directory —
/// so that release reclaims no space, and the caller sees the file's page count
/// stay flat rather than drop. Every subsequent release is recorded in that
/// page.
pub fn release_page<S: FreeListStore + ?Sized>(
    store: &mut S,
    file: DataFile,
    page_id: PageId,
) -> Result<()> {
    // Hold the first free page in metadata rather than spending it on a
    // directory. Releasing one page and reallocating it then costs nothing and
    // touches no page at all.
    if store.meta_ref().free_spare_page(file) == FREE_SPARE_EMPTY {
        store.meta_mut_ref().set_free_spare_page(file, page_id);
        increment_free_count(store, file);
        return Ok(());
    }

    let head = store.meta_ref().free_directory_head(file);

    if head == FREE_DIRECTORY_EMPTY {
        // A second page needs recording and there is no directory yet. The
        // page being released becomes that directory, and the metadata-held
        // page stays where it is — evicting it instead would put a page back
        // on the slow path for no gain.
        let dir = FreeDirectoryPage::empty();
        let encoded = free_directory_codec::encode(&dir, file_magic(file))?;
        store.write_page_raw(file, page_id, &encoded)?;
        store.meta_mut_ref().set_free_directory_head(file, page_id);
        return Ok(());
    }

    let buf = store.read_page_raw(file, head)?;
    let mut dir = free_directory_codec::decode(&buf, file.file_name(), head)?;

    if dir.has_room() {
        dir.entries.push(page_id);
        let encoded = free_directory_codec::encode(&dir, file_magic(file))?;
        store.write_page_raw(file, head, &encoded)?;
        increment_free_count(store, file);
        return Ok(());
    }

    // Head is full: the released page becomes a new head linking to the old
    // one. Again no extra allocation — the page we were handed does the job.
    let new_head = FreeDirectoryPage { next: head, entries: Vec::new() };
    let encoded = free_directory_codec::encode(&new_head, file_magic(file))?;
    store.write_page_raw(file, page_id, &encoded)?;
    store.meta_mut_ref().set_free_directory_head(file, page_id);
    Ok(())
}

/// Saturating so a miscounted release can never wrap the counter to ~4 billion,
/// which would advertise a file as almost entirely reusable.
///
/// Saturating bounds the arithmetic; it does NOT make the count accurate. The
/// tally is maintained independently of the directory pages and the metadata
/// spare slot, and nothing reconciles the two — deliberately, since walking the
/// chain to count is the I/O this design exists to avoid. So the figure
/// [`crate::Graph::reusable_overflow_page_count`] reports is an estimate that a
/// crash before flush can leave stale (see the known limit documented on
/// `FileBackend`'s `FreeListStore` impl). It is safe to be wrong: the count is
/// only ever read for reporting, never to decide whether a page may be handed
/// out — that decision reads the directory itself.
fn decrement_free_count<S: FreeListStore + ?Sized>(store: &mut S, file: DataFile) {
    let current = store.meta_ref().free_page_count(file);
    store.meta_mut_ref().set_free_page_count(file, current.saturating_sub(1));
}

fn increment_free_count<S: FreeListStore + ?Sized>(store: &mut S, file: DataFile) {
    let current = store.meta_ref().free_page_count(file);
    store.meta_mut_ref().set_free_page_count(file, current.saturating_add(1));
}
