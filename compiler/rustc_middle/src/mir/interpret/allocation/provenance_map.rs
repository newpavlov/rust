//! Store the provenance for each byte in the range, with a more efficient
//! representation for the common case where PTR_SIZE consecutive bytes have the same provenance.

use std::cmp;

use rustc_data_structures::sorted_map::SortedMap;
use rustc_target::abi::{HasDataLayout, Size};

use super::{alloc_range, AllocError, AllocId, AllocRange, AllocResult, Provenance};

/// Stores the provenance information of pointers stored in memory.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, TyEncodable, TyDecodable)]
#[derive(HashStable)]
pub struct ProvenanceMap<Prov = AllocId> {
    /// Provenance in this map applies from the given offset for an entire pointer-size worth of
    /// bytes. Two entires in this map are always at least a pointer size apart.
    ptrs: SortedMap<Size, Prov>,
    /// Provenance in this map only applies to the given single byte.
    /// This map is disjoint from the previous. It will always be empty when
    /// `Prov::OFFSET_IS_ADDR` is false.
    bytes: SortedMap<Size, Prov>,
}

impl<Prov> ProvenanceMap<Prov> {
    pub fn new() -> Self {
        ProvenanceMap { ptrs: SortedMap::new(), bytes: SortedMap::new() }
    }

    /// The caller must guarantee that the given provenance list is already sorted
    /// by address and contain no duplicates.
    pub fn from_presorted_ptrs(r: Vec<(Size, Prov)>) -> Self {
        ProvenanceMap { ptrs: SortedMap::from_presorted_elements(r), bytes: SortedMap::new() }
    }
}

impl ProvenanceMap {
    /// Give access to the ptr-sized provenances (which can also be thought of as relocations, and
    /// indeed that is how codegen treats them).
    ///
    /// Only exposed with `AllocId` provenance, since it panics if there is bytewise provenance.
    #[inline]
    pub fn ptrs(&self) -> &SortedMap<Size, AllocId> {
        debug_assert!(self.bytes.is_empty()); // `AllocId::OFFSET_IS_ADDR` is false so this cannot fail
        &self.ptrs
    }
}

impl<Prov: Provenance> ProvenanceMap<Prov> {
    /// Returns all ptr-sized provenance in the given range.
    /// If the range has length 0, returns provenance that crosses the edge between `start-1` and
    /// `start`.
    fn range_get_ptrs(&self, range: AllocRange, cx: &impl HasDataLayout) -> &[(Size, Prov)] {
        // We have to go back `pointer_size - 1` bytes, as that one would still overlap with
        // the beginning of this range.
        let adjusted_start = Size::from_bytes(
            range.start.bytes().saturating_sub(cx.data_layout().pointer_size.bytes() - 1),
        );
        self.ptrs.range(adjusted_start..range.end())
    }

    /// Returns all byte-wise provenance in the given range.
    fn range_get_bytes(&self, range: AllocRange) -> &[(Size, Prov)] {
        self.bytes.range(range.start..range.end())
    }

    /// Get the provenance of a single byte.
    pub fn get(&self, offset: Size, cx: &impl HasDataLayout) -> Option<Prov> {
        let prov = self.range_get_ptrs(alloc_range(offset, Size::from_bytes(1)), cx);
        debug_assert!(prov.len() <= 1);
        if let Some(entry) = prov.first() {
            // If it overlaps with this byte, it is on this byte.
            debug_assert!(self.bytes.get(&offset).is_none());
            Some(entry.1)
        } else {
            // Look up per-byte provenance.
            self.bytes.get(&offset).copied()
        }
    }

    /// Check if here is ptr-sized provenance at the given index.
    /// Does not mean anything for bytewise provenance! But can be useful as an optimization.
    pub fn get_ptr(&self, offset: Size) -> Option<Prov> {
        self.ptrs.get(&offset).copied()
    }

    /// Returns whether this allocation has provenance overlapping with the given range.
    ///
    /// Note: this function exists to allow `range_get_provenance` to be private, in order to somewhat
    /// limit access to provenance outside of the `Allocation` abstraction.
    ///
    pub fn range_empty(&self, range: AllocRange, cx: &impl HasDataLayout) -> bool {
        self.range_get_ptrs(range, cx).is_empty() && self.range_get_bytes(range).is_empty()
    }

    /// Yields all the provenances stored in this map.
    pub fn provenances(&self) -> impl Iterator<Item = Prov> + '_ {
        self.ptrs.values().chain(self.bytes.values()).copied()
    }

    pub fn insert_ptr(&mut self, offset: Size, prov: Prov, cx: &impl HasDataLayout) {
        debug_assert!(self.range_empty(alloc_range(offset, cx.data_layout().pointer_size), cx));
        self.ptrs.insert(offset, prov);
    }

    /// Removes all provenance inside the given range.
    /// If there is provenance overlapping with the edges, might result in an error.
    pub fn clear(&mut self, range: AllocRange, cx: &impl HasDataLayout) -> AllocResult {
        let start = range.start;
        let end = range.end();
        // Clear the bytewise part -- this is easy.
        if Prov::OFFSET_IS_ADDR {
            self.bytes.remove_range(start..end);
        } else {
            debug_assert!(self.bytes.is_empty());
        }

        // For the ptr-sized part, find the first (inclusive) and last (exclusive) byte of
        // provenance that overlaps with the given range.
        let (first, last) = {
            // Find all provenance overlapping the given range.
            let provenance = self.range_get_ptrs(range, cx);
            if provenance.is_empty() {
                // No provenance in this range, we are done.
                return Ok(());
            }

            (
                provenance.first().unwrap().0,
                provenance.last().unwrap().0 + cx.data_layout().pointer_size,
            )
        };

        // We need to handle clearing the provenance from parts of a pointer.
        if first < start {
            if !Prov::OFFSET_IS_ADDR {
                // We can't split up the provenance into less than a pointer.
                return Err(AllocError::PartialPointerOverwrite(first));
            }
            // Insert the remaining part in the bytewise provenance.
            let prov = self.ptrs[&first];
            for offset in first..start {
                self.bytes.insert(offset, prov);
            }
        }
        if last > end {
            let begin_of_last = last - cx.data_layout().pointer_size;
            if !Prov::OFFSET_IS_ADDR {
                // We can't split up the provenance into less than a pointer.
                return Err(AllocError::PartialPointerOverwrite(begin_of_last));
            }
            // Insert the remaining part in the bytewise provenance.
            let prov = self.ptrs[&begin_of_last];
            for offset in end..last {
                self.bytes.insert(offset, prov);
            }
        }

        // Forget all the provenance.
        // Since provenance do not overlap, we know that removing until `last` (exclusive) is fine,
        // i.e., this will not remove any other provenance just after the ones we care about.
        self.ptrs.remove_range(first..last);

        Ok(())
    }
}

/// A partial, owned list of provenance to transfer into another allocation.
///
/// Offsets are already adjusted to the destination allocation.
pub struct ProvenanceCopy<Prov> {
    dest_ptrs: Vec<(Size, Prov)>,
    dest_bytes: Vec<(Size, Prov)>,
}

impl<Prov: Provenance> ProvenanceMap<Prov> {
    pub fn prepare_copy(
        &self,
        src: AllocRange,
        dest: Size,
        count: u64,
        cx: &impl HasDataLayout,
    ) -> AllocResult<ProvenanceCopy<Prov>> {
        let shift_offset = move |idx, offset| {
            // compute offset for current repetition
            let dest_offset = dest + src.size * idx; // `Size` operations
            // shift offsets from source allocation to destination allocation
            (offset - src.start) + dest_offset // `Size` operations
        };
        let ptr_size = cx.data_layout().pointer_size;

        // # Pointer-sized provenances
        // Get the provenances that are entirely within this range.
        // (Different from `range_get_ptrs` which asks if they overlap the range.)
        let ptrs = if src.size < ptr_size {
            // This isn't even large enough to contain a pointer.
            &[]
        } else {
            let adjusted_end =
                Size::from_bytes(src.end().bytes().saturating_sub(ptr_size.bytes() - 1));
            self.ptrs.range(src.start..adjusted_end)
        };

        // Buffer for the new list.
        let mut dest_ptrs = Vec::with_capacity(ptrs.len() * (count as usize));
        // If `count` is large, this is rather wasteful -- we are allocating a big array here, which
        // is mostly filled with redundant information since it's just N copies of the same `Prov`s
        // at slightly adjusted offsets. The reason we do this is so that in `mark_provenance_range`
        // we can use `insert_presorted`. That wouldn't work with an `Iterator` that just produces
        // the right sequence of provenance for all N copies.
        // Basically, this large array would have to be created anyway in the target allocation.
        for i in 0..count {
            dest_ptrs.extend(ptrs.iter().map(|&(offset, reloc)| (shift_offset(i, offset), reloc)));
        }

        // # Byte-sized provenances
        let mut bytes = Vec::new();
        // First, if there is a part of a pointer at the start, add that.
        if let Some(entry) = self.range_get_ptrs(alloc_range(src.start, Size::ZERO), cx).first() {
            if !Prov::OFFSET_IS_ADDR {
                // We can't split up the provenance into less than a pointer.
                return Err(AllocError::PartialPointerCopy(entry.0));
            }
            trace!("start overlapping entry: {entry:?}");
            // For really small copies, make sure we don't run off the end of the `src` range.
            let entry_end = cmp::min(entry.0 + ptr_size, src.end());
            for offset in src.start..entry_end {
                bytes.push((offset, entry.1));
            }
        } else {
            trace!("no start overlapping entry");
        }
        // Then the main part, bytewise provenance from `self.bytes`.
        if Prov::OFFSET_IS_ADDR {
            bytes.extend(self.bytes.range(src.start..src.end()));
        } else {
            debug_assert!(self.bytes.is_empty());
        }
        // And finally possibly parts of a pointer at the end.
        if let Some(entry) = self.range_get_ptrs(alloc_range(src.end(), Size::ZERO), cx).first() {
            if !Prov::OFFSET_IS_ADDR {
                // We can't split up the provenance into less than a pointer.
                return Err(AllocError::PartialPointerCopy(entry.0));
            }
            trace!("end overlapping entry: {entry:?}");
            // For really small copies, make sure we don't start before `src` does.
            let entry_start = cmp::max(entry.0, src.start);
            for offset in entry_start..src.end() {
                if bytes.last().map_or(true, |bytes_entry| bytes_entry.0 < offset) {
                    // The last entry, if it exists, has a lower offset than us.
                    bytes.push((offset, entry.1));
                } else {
                    // There already is an entry for this offset in there! This can happen when the
                    // start and end range checks actually end up hitting the same pointer, so we
                    // already added this in the "pointer at the start" part above.
                    assert!(entry.0 <= src.start);
                }
            }
        } else {
            trace!("no end overlapping entry");
        }
        trace!("byte provenances: {bytes:?}");

        // And again a buffer for the new list on the target side.
        let mut dest_bytes = Vec::with_capacity(bytes.len() * (count as usize));
        for i in 0..count {
            dest_bytes
                .extend(bytes.iter().map(|&(offset, reloc)| (shift_offset(i, offset), reloc)));
        }

        Ok(ProvenanceCopy { dest_ptrs, dest_bytes })
    }

    /// Applies a provenance copy.
    /// The affected range, as defined in the parameters to `prepare_copy` is expected
    /// to be clear of provenance.
    pub fn apply_copy(&mut self, copy: ProvenanceCopy<Prov>) {
        self.ptrs.insert_presorted(copy.dest_ptrs);
        if Prov::OFFSET_IS_ADDR {
            self.bytes.insert_presorted(copy.dest_bytes);
        } else {
            debug_assert!(copy.dest_bytes.is_empty());
        }
    }
}
