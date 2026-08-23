// SPDX-License-Identifier: MIT

use std::collections::HashMap;

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::meta::GraphMeta;
use crate::storage::page::{PageBuf, new_page_buf};

/// In-memory storage backend. All pages live in a `HashMap`.
///
/// Used by `Graph::new()` for ephemeral graphs that don't need persistence.
/// `flush()` is a no-op since there is no backing store.
pub struct MemoryBackend {
    pages: HashMap<(DataFile, PageId), PageBuf>,
    page_counts: HashMap<DataFile, u32>,
    meta: GraphMeta,
}

impl MemoryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            page_counts: HashMap::new(),
            meta: GraphMeta::new(),
        }
    }

    fn file_page_count(&self, file: DataFile) -> u32 {
        self.page_counts.get(&file).copied().unwrap_or(0)
    }

    const fn set_meta_page_count(&mut self, file: DataFile, count: u32) {
        match file {
            DataFile::Nodes => self.meta.nodes_page_count = count,
            DataFile::Edges => self.meta.edges_page_count = count,
            DataFile::Adjacency => self.meta.adj_page_count = count,
            DataFile::Strings => self.meta.strings_page_count = count,
            DataFile::Overflow => self.meta.overflow_page_count = count,
        }
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBackend for MemoryBackend {
    fn read_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
        self.pages
            .get(&(file, page_id))
            .map(|p| {
                let mut copy = new_page_buf();
                copy.copy_from_slice(p.as_ref());
                copy
            })
            .ok_or_else(|| Error::CorruptPage {
                file: file.file_name(),
                page_id,
                reason: "page not allocated",
            })
    }

    fn write_page(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()> {
        let count = self.file_page_count(file);
        if page_id >= count {
            return Err(Error::CorruptPage {
                file: file.file_name(),
                page_id,
                reason: "page not allocated",
            });
        }
        let mut buf = new_page_buf();
        buf.copy_from_slice(data.as_ref());
        self.pages.insert((file, page_id), buf);
        Ok(())
    }

    fn allocate_page(&mut self, file: DataFile) -> Result<PageId> {
        // Reuse before growing: a workload that rewrites the same entities
        // must not extend the file on every rewrite.
        if let Some(recycled) = crate::storage::free_list::take_free_page(self, file)? {
            // Hand back a blank page so a recycled one cannot be mistaken for
            // its previous occupant by a caller that reads before writing.
            self.pages.insert((file, recycled), new_page_buf());
            return Ok(recycled);
        }

        let count = self.page_counts.entry(file).or_insert(0);
        let page_id = *count;
        *count += 1;
        self.set_meta_page_count(file, page_id + 1);

        // Insert a zeroed page
        let buf = new_page_buf();
        self.pages.insert((file, page_id), buf);
        Ok(page_id)
    }

    fn free_page(&mut self, file: DataFile, page_id: PageId) -> Result<()> {
        crate::storage::free_list::release_page(self, file, page_id)
    }

    fn page_count(&self, file: DataFile) -> u32 {
        self.file_page_count(file)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn meta(&self) -> &GraphMeta {
        &self.meta
    }

    fn meta_mut(&mut self) -> &mut GraphMeta {
        &mut self.meta
    }

    fn read_index_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_index_bytes(&mut self, _data: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// Gives the shared free-list algorithm raw page access, bypassing this
/// backend's own `allocate_page` (which calls into that algorithm, so routing
/// back through it would recurse).
impl crate::storage::free_list::FreeListStore for MemoryBackend {
    fn read_page_raw(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
        self.read_page(file, page_id)
    }

    fn write_page_raw(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()> {
        let mut buf = new_page_buf();
        buf.copy_from_slice(data.as_ref());
        self.pages.insert((file, page_id), buf);
        Ok(())
    }

    fn meta_ref(&self) -> &GraphMeta {
        &self.meta
    }

    fn meta_mut_ref(&mut self) -> &mut GraphMeta {
        &mut self.meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::PAGE_SIZE;

    #[test]
    fn memory_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MemoryBackend>();
    }

    #[test]
    fn allocate_page_returns_sequential_ids() {
        let mut backend = MemoryBackend::new();
        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 0);
        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 1);
        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 2);
    }

    #[test]
    fn allocate_page_increments_count() {
        let mut backend = MemoryBackend::new();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Nodes).unwrap();
        assert_eq!(backend.page_count(DataFile::Nodes), 3);
    }

    #[test]
    fn write_then_read_page() {
        let mut backend = MemoryBackend::new();
        let page_id = backend.allocate_page(DataFile::Nodes).unwrap();

        let mut data = new_page_buf();
        data[0] = 0xAA;
        data[100] = 0xBB;
        data[PAGE_SIZE - 1] = 0xCC;
        backend.write_page(DataFile::Nodes, page_id, &data).unwrap();

        let read_back = backend.read_page(DataFile::Nodes, page_id).unwrap();
        assert_eq!(read_back[0], 0xAA);
        assert_eq!(read_back[100], 0xBB);
        assert_eq!(read_back[PAGE_SIZE - 1], 0xCC);
        assert_eq!(read_back.as_ref(), data.as_ref());
    }

    #[test]
    fn read_unallocated_page_errors() {
        let backend = MemoryBackend::new();
        assert!(backend.read_page(DataFile::Nodes, 0).is_err());
    }

    #[test]
    fn write_unallocated_page_errors() {
        let mut backend = MemoryBackend::new();
        let data = new_page_buf();
        assert!(backend.write_page(DataFile::Nodes, 0, &data).is_err());
    }

    #[test]
    fn independent_file_page_counts() {
        let mut backend = MemoryBackend::new();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Edges).unwrap();
        backend.allocate_page(DataFile::Edges).unwrap();

        assert_eq!(backend.page_count(DataFile::Nodes), 3);
        assert_eq!(backend.page_count(DataFile::Edges), 2);
        assert_eq!(backend.page_count(DataFile::Adjacency), 0);
    }

    #[test]
    fn overwrite_page() {
        let mut backend = MemoryBackend::new();
        let page_id = backend.allocate_page(DataFile::Edges).unwrap();

        let mut data1 = new_page_buf();
        data1[0] = 0x11;
        backend
            .write_page(DataFile::Edges, page_id, &data1)
            .unwrap();

        let mut data2 = new_page_buf();
        data2[0] = 0x22;
        backend
            .write_page(DataFile::Edges, page_id, &data2)
            .unwrap();

        let read_back = backend.read_page(DataFile::Edges, page_id).unwrap();
        assert_eq!(read_back[0], 0x22);
    }

    #[test]
    fn flush_is_noop() {
        let mut backend = MemoryBackend::new();
        assert!(backend.flush().is_ok());
    }

    #[test]
    fn meta_initial_values() {
        let backend = MemoryBackend::new();
        let m = backend.meta();
        assert_eq!(m.next_node_id, 1);
        assert_eq!(m.next_edge_id, 1);
        assert_eq!(m.node_count, 0);
        assert_eq!(m.edge_count, 0);
        assert_eq!(m.nodes_page_count, 0);
        assert_eq!(m.edges_page_count, 0);
        assert_eq!(m.adj_page_count, 0);
        assert_eq!(m.strings_page_count, 0);
        assert_eq!(m.overflow_page_count, 0);
    }

    #[test]
    fn meta_mut_modifiable() {
        let mut backend = MemoryBackend::new();
        backend.meta_mut().next_node_id = 42;
        assert_eq!(backend.meta().next_node_id, 42);
    }

    #[test]
    fn all_data_file_variants() {
        let mut backend = MemoryBackend::new();
        let files = [
            DataFile::Nodes,
            DataFile::Edges,
            DataFile::Adjacency,
            DataFile::Strings,
            DataFile::Overflow,
        ];

        for (i, &file) in files.iter().enumerate() {
            let page_id = backend.allocate_page(file).unwrap();
            let mut data = new_page_buf();
            // Test fixture: `i` indexes a 5-element array.
            #[allow(clippy::cast_possible_truncation)]
            let marker = (i + 1) as u8;
            data[0] = marker;
            backend.write_page(file, page_id, &data).unwrap();
        }

        for (i, &file) in files.iter().enumerate() {
            let read_back = backend.read_page(file, 0).unwrap();
            // Test fixture: same 5-element array as the write above.
            #[allow(clippy::cast_possible_truncation)]
            let marker = (i + 1) as u8;
            assert_eq!(read_back[0], marker);
            assert_eq!(backend.page_count(file), 1);
        }

        // Verify meta page counts updated
        let m = backend.meta();
        assert_eq!(m.nodes_page_count, 1);
        assert_eq!(m.edges_page_count, 1);
        assert_eq!(m.adj_page_count, 1);
        assert_eq!(m.strings_page_count, 1);
        assert_eq!(m.overflow_page_count, 1);
    }
}
