use crate::borrow::Exclusive;
use crate::borrow::Shared;
use crate::borrow::{AtomicRefCell, Ref, RefMap, RefMapMut, RefMut};
use crate::entity::Entity;
use crate::entity::EntityLocation;
use crate::filter::ArchetypeFilterData;
use crate::filter::Filter;
use crate::world::TagSet;
use crate::world::WorldId;
use derivative::Derivative;
use smallvec::Drain;
use smallvec::SmallVec;
use std::any::TypeId;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::iter::Zip;
use std::mem::size_of;
use std::num::Wrapping;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::RangeBounds;
use std::ptr::NonNull;
use std::slice::Iter;
use std::slice::IterMut;

/// A type ID identifying a component type.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ComponentTypeId(TypeId);

impl ComponentTypeId {
    /// Gets the component type ID that represents type `T`.
    pub fn of<T: Component>() -> Self {
        ComponentTypeId(TypeId::of::<T>())
    }
}

/// A type ID identifying a tag type.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct TagTypeId(TypeId);

impl TagTypeId {
    /// Gets the tag type ID that represents type `T`.
    pub fn of<T: Tag>() -> Self {
        TagTypeId(TypeId::of::<T>())
    }
}

/// A `Component` is per-entity data that can be attached to a single entity.
pub trait Component: Send + Sync + 'static {}

/// A `Tag` is shared data that can be attached to multiple entities at once.
pub trait Tag: Clone + Send + Sync + PartialEq + 'static {}

impl<T: Send + Sync + 'static> Component for T {}
impl<T: Clone + Send + Sync + PartialEq + 'static> Tag for T {}

/// Stores slices of `ComponentTypeId`, each of which identifies the type of components
/// contained within the archetype of the same index.
#[derive(Derivative)]
#[derivative(Default(bound = ""))]
pub struct ComponentTypes(SliceVec<ComponentTypeId>);

/// Stores slices of `TagTypeId`, each of which identifies the type of tags
/// contained within the archetype of the same index.
#[derive(Derivative)]
#[derivative(Default(bound = ""))]
pub struct TagTypes(SliceVec<TagTypeId>);

impl ComponentTypes {
    /// Gets an iterator over all type ID slices.
    pub fn iter(&self) -> SliceVecIter<ComponentTypeId> {
        self.0.iter()
    }

    /// Gets the number of slices stored within the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Determines if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.len() < 1
    }
}

impl TagTypes {
    /// Gets an iterator over all type ID slices.
    pub fn iter(&self) -> SliceVecIter<TagTypeId> {
        self.0.iter()
    }

    /// Gets the number of slices stored within the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Determines if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.len() < 1
    }
}

/// A vector of slices.
///
/// Each slice is stored inline so as to be efficiently iterated through linearly.
#[derive(Derivative)]
#[derivative(Default(bound = ""))]
pub struct SliceVec<T> {
    data: Vec<T>,
    counts: Vec<usize>,
}

impl<T> SliceVec<T> {
    /// Gets the length of the vector.
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    /// Determines if the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len() < 1
    }

    /// Pushes a new slice onto the end of the vector.
    pub fn push<I: IntoIterator<Item = T>>(&mut self, items: I) {
        let mut count = 0;
        for item in items.into_iter() {
            self.data.push(item);
            count += 1;
        }
        self.counts.push(count);
    }

    /// Gets an iterator over all slices in the vector.
    pub fn iter(&self) -> SliceVecIter<T> {
        SliceVecIter {
            data: &self.data,
            counts: &self.counts,
        }
    }
}

/// An iterator over slices in a `SliceVec`.
pub struct SliceVecIter<'a, T> {
    data: &'a [T],
    counts: &'a [usize],
}

impl<'a, T> Iterator for SliceVecIter<'a, T> {
    type Item = &'a [T];

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((count, remaining_counts)) = self.counts.split_first() {
            let (data, remaining_data) = self.data.split_at(*count);
            self.counts = remaining_counts;
            self.data = remaining_data;
            Some(data)
        } else {
            None
        }
    }
}

/// Stores all entity data for a `World`.
pub struct Storage {
    world_id: WorldId,
    component_types: ComponentTypes,
    tag_types: TagTypes,
    archetypes: Vec<ArchetypeData>,
}

impl Storage {
    // Creates an empty `Storage`.
    pub fn new(world_id: WorldId) -> Self {
        Self {
            world_id,
            component_types: ComponentTypes::default(),
            tag_types: TagTypes::default(),
            archetypes: Vec::default(),
        }
    }

    /// Creates a new archetype.
    ///
    /// Returns the index of the newly created archetype and an exclusive reference to the
    /// achetype's data.
    pub fn alloc_archetype(&mut self, desc: ArchetypeDescription) -> (usize, &mut ArchetypeData) {
        self.component_types
            .0
            .push(desc.components.iter().map(|(type_id, _)| *type_id));
        self.tag_types
            .0
            .push(desc.tags.iter().map(|(type_id, _)| *type_id));
        self.archetypes.push(ArchetypeData::new(
            ArchetypeId(self.world_id, self.archetypes.len()),
            desc,
        ));

        let index = self.archetypes.len() - 1;
        (index, unsafe {
            self.archetypes_mut().get_unchecked_mut(index)
        })
    }

    pub(crate) fn push(&mut self, archetype: ArchetypeData) {
        let desc = archetype.description();
        self.component_types
            .0
            .push(desc.components.iter().map(|(t, _)| *t));
        self.tag_types.0.push(desc.tags.iter().map(|(t, _)| *t));
        self.archetypes.push(archetype);
    }

    /// Gets a vector of slices of all component types for all archetypes.
    ///
    /// Each slice contains the component types for the archetype at the corresponding index.
    pub fn component_types(&self) -> &ComponentTypes {
        &self.component_types
    }

    /// Gets a vector of slices of all tag types for all archetypes.
    ///
    /// Each slice contains the tag types for the archetype at the corresponding index.
    pub fn tag_types(&self) -> &TagTypes {
        &self.tag_types
    }

    /// Gets a slice reference to all archetypes.
    pub fn archetypes(&self) -> &[ArchetypeData] {
        &self.archetypes
    }

    /// Gets a mutable slice reference to all archetypes.
    pub fn archetypes_mut(&mut self) -> &mut [ArchetypeData] {
        &mut self.archetypes
    }

    pub(crate) fn drain<R: RangeBounds<usize>>(
        &mut self,
        range: R,
    ) -> std::vec::Drain<ArchetypeData> {
        self.archetypes.drain(range)
    }
}

/// Stores metadata decribing the type of a tag.
#[derive(Copy, Clone)]
pub struct TagMeta {
    size: usize,
    align: usize,
    drop_fn: Option<(fn(*mut u8))>,
    eq_fn: fn(*const u8, *const u8) -> bool,
    clone_fn: fn(*const u8, *mut u8),
}

impl TagMeta {
    /// Gets the tag meta of tag type `T`.
    pub fn of<T: Tag>() -> Self {
        TagMeta {
            size: size_of::<T>(),
            align: std::mem::align_of::<T>(),
            drop_fn: Some(|ptr| unsafe { std::ptr::drop_in_place(ptr as *mut T) }),
            eq_fn: |a, b| unsafe { *(a as *const T) == *(b as *const T) },
            clone_fn: |src, dst| unsafe {
                let clone = (&*(src as *const T)).clone();
                std::ptr::write(dst as *mut T, clone);
            },
        }
    }

    pub(crate) fn equals(&self, a: *const u8, b: *const u8) -> bool {
        (self.eq_fn)(a, b)
    }

    pub(crate) fn clone(&self, src: *const u8, dst: *mut u8) {
        (self.clone_fn)(src, dst)
    }

    pub(crate) fn layout(&self) -> std::alloc::Layout {
        unsafe { std::alloc::Layout::from_size_align_unchecked(self.size, self.align) }
    }

    pub(crate) fn is_zero_sized(&self) -> bool {
        self.size == 0
    }
}

/// Stores metadata describing the type of a component.
#[derive(Copy, Clone)]
pub struct ComponentMeta {
    size: usize,
    align: usize,
    drop_fn: Option<(fn(*mut u8))>,
}

impl ComponentMeta {
    /// Gets the component meta of component type `T`.
    pub fn of<T: Component>() -> Self {
        ComponentMeta {
            size: size_of::<T>(),
            align: std::mem::align_of::<T>(),
            drop_fn: Some(|ptr| unsafe { std::ptr::drop_in_place(ptr as *mut T) }),
        }
    }
}

/// Describes the layout of an archetype, including what components
/// and tags shall be attached to entities stored within an archetype.
#[derive(Default, Clone)]
pub struct ArchetypeDescription {
    tags: Vec<(TagTypeId, TagMeta)>,
    components: Vec<(ComponentTypeId, ComponentMeta)>,
}

impl ArchetypeDescription {
    /// Gets a slice of the tags in the description.
    pub fn tags(&self) -> &[(TagTypeId, TagMeta)] {
        &self.tags
    }

    /// Gets a slice of the components in the description.
    pub fn components(&self) -> &[(ComponentTypeId, ComponentMeta)] {
        &self.components
    }

    /// Adds a tag to the description.
    pub fn register_tag_raw(&mut self, type_id: TagTypeId, type_meta: TagMeta) {
        self.tags.push((type_id, type_meta));
    }

    /// Adds a tag to the description.
    pub fn register_tag<T: Tag>(&mut self) {
        self.register_tag_raw(TagTypeId(TypeId::of::<T>()), TagMeta::of::<T>());
    }

    /// Adds a component to the description.
    pub fn register_component_raw(&mut self, type_id: ComponentTypeId, type_meta: ComponentMeta) {
        self.components.push((type_id, type_meta));
    }

    /// Adds a component to the description.
    pub fn register_component<T: Component>(&mut self) {
        self.register_component_raw(ComponentTypeId(TypeId::of::<T>()), ComponentMeta::of::<T>());
    }
}

impl<'a> Filter<ArchetypeFilterData<'a>> for ArchetypeDescription {
    type Iter = Zip<SliceVecIter<'a, TagTypeId>, SliceVecIter<'a, ComponentTypeId>>;

    fn collect(&self, source: ArchetypeFilterData<'a>) -> Self::Iter {
        source.tag_types.iter().zip(source.component_types.iter())
    }

    fn is_match(&mut self, (tags, components): &<Self::Iter as Iterator>::Item) -> Option<bool> {
        Some(
            tags.len() == self.tags.len()
                && self.tags.iter().all(|(t, _)| tags.contains(t))
                && components.len() == self.components.len()
                && self.components.iter().all(|(t, _)| components.contains(t)),
        )
    }
}

const MAX_CHUNK_SIZE: usize = 16 * 1024;
const COMPONENT_STORAGE_ALIGNMENT: usize = 64;

/// Unique ID of an archetype.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ArchetypeId(WorldId, usize);

impl ArchetypeId {
    pub(crate) fn new(world_id: WorldId, index: usize) -> Self {
        ArchetypeId(world_id, index)
    }

    fn index(self) -> usize {
        self.1
    }

    fn world(self) -> WorldId {
        self.0
    }
}

/// Contains all of the tags attached to the entities in each chunk.
pub struct Tags(SmallVec<[(TagTypeId, TagStorage); 3]>);

impl Tags {
    fn new(mut data: SmallVec<[(TagTypeId, TagStorage); 3]>) -> Self {
        data.sort_by_key(|(t, _)| *t);
        Self(data)
    }

    fn validate(&self, set_count: usize) {
        for (_, tags) in self.0.iter() {
            debug_assert_eq!(set_count, tags.len());
        }
    }

    /// Gets the set of tag values of the specified type attached to all chunks.
    #[inline]
    pub fn get(&self, type_id: TagTypeId) -> Option<&TagStorage> {
        self.0
            .binary_search_by_key(&type_id, |(t, _)| *t)
            .ok()
            .map(|i| unsafe { &self.0.get_unchecked(i).1 })
    }

    /// Mutably gets the set of all tag values of the specified type attached to all chunks.
    #[inline]
    pub fn get_mut(&mut self, type_id: TagTypeId) -> Option<&mut TagStorage> {
        self.0
            .binary_search_by_key(&type_id, |(t, _)| *t)
            .ok()
            .map(move |i| unsafe { &mut self.0.get_unchecked_mut(i).1 })
    }

    pub(crate) fn tag_set(&self, chunk: usize) -> DynamicTagSet {
        let mut tags = DynamicTagSet { tags: Vec::new() };

        unsafe {
            for (type_id, storage) in self.0.iter() {
                let (ptr, _, count) = storage.data_raw();
                debug_assert!(chunk < count, "chunk index out of bounds");
                tags.push(*type_id, *storage.element(), ptr);
            }
        }

        tags
    }
}

pub(crate) struct DynamicTagSet {
    // the pointer here is to heap allocated memory owned by the tag set
    tags: Vec<(TagTypeId, TagMeta, NonNull<u8>)>,
}

unsafe impl Send for DynamicTagSet {}

unsafe impl Sync for DynamicTagSet {}

impl DynamicTagSet {
    pub fn push(&mut self, type_id: TagTypeId, meta: TagMeta, value: NonNull<u8>) {
        // we clone the value here and take ownership of the copy
        unsafe {
            if meta.is_zero_sized() {
                self.tags
                    .push((type_id, meta, NonNull::new(meta.align as *mut u8).unwrap()));
            } else {
                let copy = std::alloc::alloc(meta.layout());
                meta.clone(value.as_ptr(), copy);
                self.tags.push((type_id, meta, NonNull::new(copy).unwrap()));
            }
        }
    }

    pub fn remove(&mut self, type_id: TagTypeId) {
        if let Some((i, _)) = self
            .tags
            .iter()
            .enumerate()
            .find(|(_, (t, _, _))| *t == type_id)
        {
            let (_, meta, ptr) = self.tags.remove(i);
            unsafe {
                // drop and dealloc the copy as we own this memory
                if let Some(drop_fn) = meta.drop_fn {
                    drop_fn(ptr.as_ptr());
                }

                if !meta.is_zero_sized() {
                    std::alloc::dealloc(ptr.as_ptr(), meta.layout());
                }
            }
        }
    }
}

impl TagSet for DynamicTagSet {
    fn write_tags(&self, tags: &mut Tags) {
        for (type_id, meta, ptr) in self.tags.iter() {
            let storage = tags.get_mut(*type_id).unwrap();
            unsafe {
                if meta.drop_fn.is_some() && !meta.is_zero_sized() {
                    // clone the value into temp storage then move it into the chunk
                    // we can dealloc the copy without dropping because the value
                    // is considered moved and will be dropped by the tag storage later
                    let copy = std::alloc::alloc(meta.layout());
                    meta.clone(ptr.as_ptr(), copy);
                    storage.push_raw(copy);
                    std::alloc::dealloc(copy, meta.layout());
                } else {
                    // copy the value directly into the tag storage
                    // if the value has no drop fn, then it is safe for us to make
                    // copies of the data without explicit clones
                    storage.push_raw(ptr.as_ptr())
                }
            }
        }
    }
}

impl Drop for DynamicTagSet {
    fn drop(&mut self) {
        // we own all of the vales in the set, so we need to drop and dealloc them
        for (_, meta, ptr) in self.tags.drain(..) {
            unsafe {
                let layout = std::alloc::Layout::from_size_align_unchecked(meta.size, meta.align);
                if let Some(drop_fn) = meta.drop_fn {
                    drop_fn(ptr.as_ptr());
                }
                if !meta.is_zero_sized() {
                    std::alloc::dealloc(ptr.as_ptr(), layout);
                }
            }
        }
    }
}

/// Stores entity data in chunks. All entities within an archetype have the same data layout
/// (component and tag types).
pub struct ArchetypeData {
    id: ArchetypeId,
    desc: ArchetypeDescription,
    tags: Tags,
    component_layout: ComponentStorageLayout,
    chunk_sets: Vec<Chunkset>,
}

impl ArchetypeData {
    fn new(id: ArchetypeId, desc: ArchetypeDescription) -> Self {
        // create tag storage
        let tags = desc
            .tags
            .iter()
            .map(|(type_id, meta)| (*type_id, TagStorage::new(*meta)))
            .collect();

        // create component data layout
        let max_component_size = desc
            .components
            .iter()
            .map(|(_, meta)| meta.size)
            .max()
            .unwrap_or(0);
        let entity_capacity = std::cmp::max(
            1,
            MAX_CHUNK_SIZE / std::cmp::max(max_component_size, size_of::<Entity>()),
        );
        let mut data_capacity = 0usize;
        let mut component_data_offsets = Vec::new();
        for (type_id, meta) in desc.components.iter() {
            data_capacity = align_up(
                align_up(data_capacity, COMPONENT_STORAGE_ALIGNMENT),
                meta.align,
            );
            component_data_offsets.push((*type_id, data_capacity, *meta));
            data_capacity += meta.size * entity_capacity;
        }
        let data_alignment =
            std::alloc::Layout::from_size_align(data_capacity, COMPONENT_STORAGE_ALIGNMENT)
                .expect("invalid component data size/alignment");

        ArchetypeData {
            desc,
            id,
            tags: Tags::new(tags),
            component_layout: ComponentStorageLayout {
                capacity: entity_capacity,
                alloc_layout: data_alignment,
                data_layout: component_data_offsets,
            },
            chunk_sets: Vec::new(),
        }
    }

    /// Gets the unique ID of this archetype.
    pub fn id(&self) -> ArchetypeId {
        self.id
    }

    pub(crate) fn merge(&mut self, mut other: ArchetypeData) {
        for (i, mut set) in other.chunk_sets.drain(..).enumerate() {
            let mut set_match = None;
            for index in 0..self.chunk_sets.len() {
                let mut matches = true;
                for (type_id, tags) in self.tags.0.iter() {
                    unsafe {
                        let (a_ptr, size, _) = tags.data_raw();
                        let (b_ptr, _, _) = other.tags.get(*type_id).unwrap().data_raw();

                        if !tags.element().equals(
                            a_ptr.as_ptr().add(index * size),
                            b_ptr.as_ptr().add(i * size),
                        ) {
                            matches = false;
                            break;
                        }
                    }
                }

                if matches {
                    set_match = Some(i);
                    break;
                }
            }

            if let Some(chunk_set) = set_match {
                let target = &mut self.chunk_sets[chunk_set];
                for chunk in set.drain(..) {
                    target.push(chunk);
                }
            } else {
                self.chunk_sets.push(set);
            }
        }
    }

    /// Allocates a new chunk set. Returns the index of the new set.
    ///
    /// `initialize` is expected to push the new chunkset's tag values onto the tags collection.
    pub(crate) fn alloc_chunk_set<F: FnMut(&mut Tags)>(&mut self, mut initialize: F) -> usize {
        self.chunk_sets.push(Chunkset::default());
        initialize(&mut self.tags);
        self.tags.validate(self.chunk_sets.len());
        self.chunk_sets.len() - 1
    }

    /// Finds a chunk with space free for at least one entity, creating one if needed.
    pub(crate) fn get_free_chunk(&mut self, set_index: usize) -> usize {
        let count = {
            let chunks = &mut self.chunk_sets[set_index];
            let len = chunks.len();
            for (i, chunk) in chunks.iter_mut().enumerate() {
                if !chunk.is_full() {
                    return i;
                }
            }
            len
        };

        let chunk = self
            .component_layout
            .alloc_storage(ChunkId(self.id, set_index, count));
        unsafe { self.chunk_sets.get_unchecked_mut(set_index).push(chunk) };
        count
    }

    /// Gets the number of chunk sets stored within this archetype.
    pub fn len(&self) -> usize {
        self.chunk_sets.len()
    }

    /// Determines whether this archetype has any chunks.
    pub fn is_empty(&self) -> bool {
        self.len() < 1
    }

    /// Gets the tag storage for all chunks in the archetype.
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Gets a slice of chunksets.
    pub fn chunksets(&self) -> &[Chunkset] {
        &self.chunk_sets
    }

    /// Gets a mutable slice of chunksets.
    pub fn chunksets_mut(&mut self) -> &mut [Chunkset] {
        &mut self.chunk_sets
    }

    /// Gets a description of the component types in the archetype.
    pub fn description(&self) -> &ArchetypeDescription {
        &self.desc
    }

    pub(crate) fn defrag<F: FnMut(Entity, EntityLocation)>(
        &mut self,
        budget: &mut usize,
        mut on_moved: F,
    ) -> bool {
        let arch_index = self.id.index();
        for (i, chunkset) in self.chunk_sets.iter_mut().enumerate() {
            let complete = chunkset.defrag(budget, |e, chunk, component| {
                on_moved(e, EntityLocation::new(arch_index, i, chunk, component));
            });
            if !complete {
                return false;
            }
        }

        true
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + (align - 1)) & align.wrapping_neg()
}

/// Describes the data layout for a chunk.
pub struct ComponentStorageLayout {
    capacity: usize,
    alloc_layout: std::alloc::Layout,
    data_layout: Vec<(ComponentTypeId, usize, ComponentMeta)>,
}

impl ComponentStorageLayout {
    /// The maximum number of entities that can be stored in each chunk.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The components in each chunk.
    pub fn components(&self) -> &[(ComponentTypeId, usize, ComponentMeta)] {
        &self.data_layout
    }

    fn alloc_storage(&self, id: ChunkId) -> ComponentStorage {
        let storage_info = self
            .data_layout
            .iter()
            .map(|(ty, _, meta)| {
                (
                    *ty,
                    ComponentAccessor {
                        ptr: AtomicRefCell::new(meta.align as *mut u8),
                        capacity: self.capacity,
                        count: UnsafeCell::new(0),
                        element_size: meta.size,
                        drop_fn: meta.drop_fn,
                        version: UnsafeCell::new(Wrapping(0)),
                        changed: UnsafeCell::new(Wrapping(false)),
                    },
                )
            })
            .collect();

        ComponentStorage {
            id,
            capacity: self.capacity,
            entities: Vec::with_capacity(self.capacity),
            component_offsets: self
                .data_layout
                .iter()
                .map(|(ty, offset, _)| (*ty, *offset))
                .collect(),
            component_layout: self.alloc_layout,
            component_info: UnsafeCell::new(Components::new(storage_info)),
            component_data: None,
        }
    }
}

/// Contains chunks with the same layout and tag values.
#[derive(Default)]
pub struct Chunkset {
    chunks: Vec<ComponentStorage>,
}

impl Deref for Chunkset {
    type Target = [ComponentStorage];

    fn deref(&self) -> &Self::Target {
        self.chunks.as_slice()
    }
}

impl DerefMut for Chunkset {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.chunks.as_mut_slice()
    }
}

impl Chunkset {
    /// Pushes a new chunk into the set.
    pub fn push(&mut self, chunk: ComponentStorage) {
        self.chunks.push(chunk);
    }

    pub(crate) fn drain<R: RangeBounds<usize>>(
        &mut self,
        range: R,
    ) -> std::vec::Drain<ComponentStorage> {
        self.chunks.drain(range)
    }

    /// Gets a slice reference to occupied chunks.
    pub fn occupied(&self) -> &[ComponentStorage] {
        let mut len = self.chunks.len();
        while len > 0 {
            if unsafe { !self.chunks.get_unchecked(len - 1).is_empty() } {
                break;
            }
            len -= 1;
        }
        let (some, _) = self.chunks.as_slice().split_at(len);
        some
    }

    /// Gets a mutable slice reference to occupied chunks.
    pub fn occupied_mut(&mut self) -> &mut [ComponentStorage] {
        let mut len = self.chunks.len();
        while len > 0 {
            if unsafe { !self.chunks.get_unchecked(len - 1).is_empty() } {
                break;
            }
            len -= 1;
        }
        let (some, _) = self.chunks.as_mut_slice().split_at_mut(len);
        some
    }

    /// Defragments all chunks within the chunkset.
    ///
    /// This will compact entities down into lower index chunks, preferring to fill one
    /// chunk before moving on to the next.
    ///
    /// `budget` determines the maximum number of entities that can be moved, and is decremented
    /// as this function moves entities.
    ///
    /// `on_moved` is called when an entity is moved, with the entity's ID, new chunk index,
    /// new component index.
    ///
    /// Returns whether or not the chunkset has been full defragmented.
    fn defrag<F: FnMut(Entity, usize, usize)>(
        &mut self,
        budget: &mut usize,
        mut on_moved: F,
    ) -> bool {
        let slice = self.occupied_mut();
        let mut first = 0;
        let mut last = slice.len() - 1;

        loop {
            // find the first chunk that is not full
            while first < last && slice[first].is_full() {
                first += 1;
            }

            // find the last chunk that is not empty
            while last > first && slice[last].is_empty() {
                last -= 1;
            }

            // exit if the cursors meet; the chunkset is defragmented
            if first == last {
                return true;
            }

            // get mut references to both chunks
            let (with_first, with_last) = slice.split_at_mut(last);
            let target = &mut with_first[first];
            let source = &mut with_last[0];

            // move as many entities as we can from the last chunk into the first
            loop {
                if *budget == 0 {
                    return false;
                }

                *budget -= 1;

                // move the last entity
                let swapped = source.move_entity(target, source.len() - 1);
                assert!(swapped.is_none());

                // notify move
                on_moved(*target.entities.last().unwrap(), first, target.len() - 1);

                // exit if we cant move any more
                if target.is_full() || source.is_empty() {
                    break;
                }
            }
        }
    }
}

/// Unique ID of a chunk.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ChunkId(ArchetypeId, usize, usize);

impl ChunkId {
    pub(crate) fn new(archetype: ArchetypeId, set: usize, index: usize) -> Self {
        ChunkId(archetype, set, index)
    }

    pub fn archetype_id(&self) -> ArchetypeId {
        self.0
    }

    pub(crate) fn set(&self) -> usize {
        self.1
    }

    pub(crate) fn index(&self) -> usize {
        self.2
    }
}

/// A set of component slices located on a chunk.
pub struct Components(SmallVec<[(ComponentTypeId, ComponentAccessor); 5]>);

impl Components {
    pub(crate) fn new(mut data: SmallVec<[(ComponentTypeId, ComponentAccessor); 5]>) -> Self {
        data.sort_by_key(|(t, _)| *t);
        Self(data)
    }

    /// Gets a component slice accessor for the specified component type.
    #[inline]
    pub fn get(&self, type_id: ComponentTypeId) -> Option<&ComponentAccessor> {
        self.0
            .binary_search_by_key(&type_id, |(t, _)| *t)
            .ok()
            .map(|i| unsafe { &self.0.get_unchecked(i).1 })
    }

    /// Gets a mutable component slice accessor for the specified component type.
    #[inline]
    pub fn get_mut(&mut self, type_id: ComponentTypeId) -> Option<&mut ComponentAccessor> {
        self.0
            .binary_search_by_key(&type_id, |(t, _)| *t)
            .ok()
            .map(move |i| unsafe { &mut self.0.get_unchecked_mut(i).1 })
    }

    fn iter(&mut self) -> Iter<(ComponentTypeId, ComponentAccessor)> {
        self.0.iter()
    }

    fn iter_mut(&mut self) -> IterMut<(ComponentTypeId, ComponentAccessor)> {
        self.0.iter_mut()
    }

    fn drain(&mut self) -> Drain<(ComponentTypeId, ComponentAccessor)> {
        self.0.drain()
    }
}

/// Stores a chunk of entities and their component data of a specific data layout.
pub struct ComponentStorage {
    id: ChunkId,
    capacity: usize,
    entities: Vec<Entity>,
    component_layout: std::alloc::Layout,
    component_offsets: HashMap<ComponentTypeId, usize>,
    component_info: UnsafeCell<Components>,
    component_data: Option<NonNull<u8>>,
}

impl ComponentStorage {
    /// Gets the unique ID of the chunk.
    pub fn id(&self) -> ChunkId {
        self.id
    }

    /// Gets the number of entities stored in the chunk.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Gets the maximum number of entities that can be stored in the chunk.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Determines if the chunk is full.
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    /// Determines if the chunk is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.len() == 0
    }

    /// Determines if the internal memory for this chunk has been allocated.
    pub fn is_allocated(&self) -> bool {
        self.component_data.is_some()
    }

    /// Gets a slice reference containing the IDs of all entities stored in the chunk.
    pub fn entities(&self) -> &[Entity] {
        self.entities.as_slice()
    }

    /// Gets a component accessor for the specified component type.
    pub fn components(&self, component_type: ComponentTypeId) -> Option<&ComponentAccessor> {
        unsafe { &*self.component_info.get() }.get(component_type)
    }

    /// Removes an entity from the chunk by swapping it with the last entry.
    ///
    /// Returns the ID of the entity which was swapped into the removed entity's position.
    pub fn swap_remove(&mut self, index: usize, drop: bool) -> Option<Entity> {
        self.entities.swap_remove(index);
        for (_, component) in unsafe { &mut *self.component_info.get() }.iter_mut() {
            component.writer().swap_remove(index, drop);
        }

        if self.entities.len() > index {
            Some(*self.entities.get(index).unwrap())
        } else {
            if self.is_empty() {
                self.free();
            }

            None
        }
    }

    /// Moves an entity from this chunk into a target chunk, moving all compatable components into
    /// the target chunk. Any components left over will be dropped.
    ///
    /// Returns the ID of the entity which was swapped into the removed entity's position.
    pub fn move_entity(&mut self, target: &mut ComponentStorage, index: usize) -> Option<Entity> {
        debug_assert!(index < self.len());
        debug_assert!(!target.is_full());
        if !target.is_allocated() {
            target.allocate();
        }

        let entity = unsafe { *self.entities.get_unchecked(index) };
        target.entities.push(entity);

        let self_components = unsafe { &mut *self.component_info.get() };
        let target_components = unsafe { &mut *target.component_info.get() };

        for (comp_type, accessor) in self_components.iter_mut() {
            if let Some(target_accessor) = target_components.get_mut(*comp_type) {
                // move the component into the target chunk
                let (ptr, element_size, _) = accessor.data_raw();
                unsafe {
                    let component = ptr.add(element_size * index);
                    target_accessor
                        .writer()
                        .push_raw(NonNull::new_unchecked(component), 1);
                }
            } else {
                // drop the component rather than move it
                unsafe { accessor.writer().drop_in_place(index) };
            }
        }

        // remove the entity from this chunk
        self.swap_remove(index, false)
    }

    /// Gets mutable references to the internal data of the chunk.
    pub fn write(&mut self) -> (&mut Vec<Entity>, &UnsafeCell<Components>) {
        if !self.is_allocated() {
            self.allocate();
        }
        (&mut self.entities, &self.component_info)
    }

    fn free(&mut self) {
        debug_assert!(self.is_allocated());
        debug_assert_eq!(0, self.len());

        self.entities.shrink_to_fit();

        // Safety Note:
        // accessors are left with pointers pointing to invalid memory (although aligned properly)
        // the slices returned from these accessors will be empty though, so no code
        // should ever dereference these pointers

        // free component memory
        unsafe {
            let ptr = self.component_data.take().unwrap();
            std::alloc::dealloc(ptr.as_ptr(), self.component_layout);
        }
    }

    fn allocate(&mut self) {
        debug_assert!(!self.is_allocated());

        self.entities.reserve_exact(self.capacity);

        unsafe {
            // allocating backing store
            let ptr = std::alloc::alloc(self.component_layout);
            self.component_data = Some(NonNull::new_unchecked(ptr));

            // update accessor pointers
            for (type_id, component) in (&mut *self.component_info.get()).iter_mut() {
                let offset = self.component_offsets.get(type_id).unwrap();
                *component.ptr.get_mut() = ptr.add(*offset);
            }
        }
    }
}

unsafe impl Sync for ComponentStorage {}

unsafe impl Send for ComponentStorage {}

impl Drop for ComponentStorage {
    fn drop(&mut self) {
        if let Some(ptr) = self.component_data {
            // run the drop functions of all components
            for (_, info) in unsafe { &mut *self.component_info.get() }.drain() {
                if let Some(drop_fn) = info.drop_fn {
                    let ptr = info.ptr.get_mut();
                    for i in 0..self.len() {
                        unsafe {
                            drop_fn(ptr.add(info.element_size * i));
                        }
                    }
                }
            }

            // free the chunk's memory
            unsafe {
                std::alloc::dealloc(ptr.as_ptr(), self.component_layout);
            }
        }
    }
}

/// Provides raw access to component data slices.
#[repr(align(64))]
pub struct ComponentAccessor {
    ptr: AtomicRefCell<*mut u8>,
    element_size: usize,
    count: UnsafeCell<usize>,
    capacity: usize,
    drop_fn: Option<fn(*mut u8)>,
    version: UnsafeCell<Wrapping<usize>>,
    changed: UnsafeCell<Wrapping<bool>>,
}

impl ComponentAccessor {
    /// Gets the version of the component slice.
    pub fn version(&self) -> usize {
        unsafe { (*self.version.get()).0 }
    }

    pub fn changed(&self) -> bool {
        unsafe { (*self.changed.get()).0 }
    }

    pub fn mark_unchanged(&self) {
        unsafe { *self.changed.get() = Wrapping(false) };
    }

    /// Gets a raw pointer to the start of the component slice.
    ///
    /// Returns a tuple containing `(pointer, element_size, count)`.
    ///
    /// # Safety
    ///
    /// Access to the component data within the slice is runtime borrow checked.
    /// This call will panic if borrowing rules are broken.
    pub fn data_raw(&self) -> (Ref<Shared, *mut u8>, usize, usize) {
        (self.ptr.get(), self.element_size, unsafe {
            *self.count.get()
        })
    }

    /// Gets a raw pointer to the start of the component slice.
    ///
    /// Returns a tuple containing `(pointer, element_size, count)`.
    ///
    /// # Safety
    ///
    /// Access to the component data within the slice is runtime borrow checked.
    /// This call will panic if borrowing rules are broken.
    pub fn data_raw_mut(&self) -> (RefMut<Exclusive, *mut u8>, usize, usize) {
        // this version increment is not thread safe
        // - but the pointer `get_mut` ensures exclusive access at runtime
        let ptr = self.ptr.get_mut();
        unsafe { *self.version.get() += Wrapping(1) };
        unsafe { *self.changed.get() = Wrapping(true) };
        (ptr, self.element_size, unsafe { *self.count.get() })
    }

    /// Gets a shared reference to the slice of components.
    ///
    /// # Safety
    ///
    /// Ensure that `T` is representative of the component data actually stored.
    ///
    /// Access to the component data within the slice is runtime borrow checked.
    /// This call will panic if borrowing rules are broken.
    pub unsafe fn data_slice<T>(&self) -> RefMap<Shared, &[T]> {
        let (ptr, _size, count) = self.data_raw();
        ptr.map_into(|ptr| std::slice::from_raw_parts(*ptr as *const _ as *const T, count))
    }

    /// Gets a mutable reference to the slice of components.
    ///
    /// # Safety
    ///
    /// Ensure that `T` is representative of the component data actually stored.
    ///
    /// Access to the component data within the slice is runtime borrow checked.
    /// This call will panic if borrowing rules are broken.
    pub unsafe fn data_slice_mut<T>(&self) -> RefMapMut<Exclusive, &mut [T]> {
        let (ptr, _size, count) = self.data_raw_mut();
        ptr.map_into(|ptr| std::slice::from_raw_parts_mut(*ptr as *mut _ as *mut T, count))
    }

    /// Creates a writer for pushing components into or removing from the vec.
    pub fn writer(&mut self) -> ComponentWriter {
        ComponentWriter::new(self)
    }
}

impl Debug for ComponentAccessor {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "ComponentAccessor {{ ptr: {:?}, element_size: {}, count: {}, capacity: {}, version: {} }}",
            *self.ptr.get(),
            self.element_size,
            unsafe { *self.count.get() },
            self.capacity,
            self.version()
        )
    }
}

/// Provides methods adding or removing components from a component vec.
pub struct ComponentWriter<'a> {
    accessor: &'a ComponentAccessor,
    ptr: RefMut<'a, Exclusive<'a>, *mut u8>,
}

impl<'a> ComponentWriter<'a> {
    fn new(accessor: &'a ComponentAccessor) -> ComponentWriter<'a> {
        Self {
            accessor,
            ptr: accessor.ptr.get_mut(),
        }
    }

    /// Pushes new components onto the end of the vec.
    ///
    /// # Safety
    ///
    /// Ensure the components pointed to by `components` are representative
    /// of the component types stored in the vec.
    ///
    /// This function will _copy_ all elements into the chunk. If the source is not `Copy`,
    /// the caller must then `mem::forget` the source such that the destructor does not run
    /// on the original data.
    pub unsafe fn push_raw(&mut self, components: NonNull<u8>, count: usize) {
        debug_assert!((*self.accessor.count.get() + count) <= self.accessor.capacity);
        std::ptr::copy_nonoverlapping(
            components.as_ptr(),
            self.ptr
                .add(*self.accessor.count.get() * self.accessor.element_size),
            count * self.accessor.element_size,
        );
        *self.accessor.count.get() += count;
        *self.accessor.version.get() += Wrapping(1);
        *self.accessor.changed.get() = Wrapping(true);
    }

    /// Pushes new components onto the end of the vec.
    ///
    /// # Safety
    ///
    /// Ensure that the type `T` is representative of the component types stored in the vec.
    ///
    /// This function will _copy_ all elements of `T` into the chunk. If `T` is not `Copy`,
    /// the caller must then `mem::forget` the source such that the destructor does not run
    /// on the original data.
    pub unsafe fn push<T: Component>(&mut self, components: &[T]) {
        self.push_raw(
            NonNull::new_unchecked(components.as_ptr() as *mut u8),
            components.len(),
        );
    }

    /// Removes the component at the specified index by swapping it with the last component.
    pub fn swap_remove(&mut self, index: usize, drop: bool) {
        unsafe {
            let size = self.accessor.element_size;
            let to_remove = self.ptr.add(size * index);
            if drop {
                if let Some(drop_fn) = self.accessor.drop_fn {
                    drop_fn(to_remove);
                }
            }

            let count = *self.accessor.count.get();
            if index < count - 1 {
                let swap_target = self.ptr.add(size * (count - 1));
                std::ptr::copy_nonoverlapping(swap_target, to_remove, size);
            }

            *self.accessor.count.get() -= 1;
        }
    }

    /// Drops the component stored at `index` without moving any other data or
    /// altering the number of elements.
    pub unsafe fn drop_in_place(&mut self, index: usize) {
        if let Some(drop_fn) = self.accessor.drop_fn {
            let size = self.accessor.element_size;
            let to_remove = self.ptr.add(size * index);
            drop_fn(to_remove);
        }
    }
}

/// A vector of tag values of a single type.
///
/// Each element in the vector represents the value of tag for
/// the chunk with the corresponding index.
pub struct TagStorage {
    ptr: NonNull<u8>,
    capacity: usize,
    len: usize,
    element: TagMeta,
}

impl TagStorage {
    fn new(element: TagMeta) -> Self {
        let capacity = if element.size == 0 { !0 } else { 4 };

        let ptr = unsafe {
            if element.size > 0 {
                let layout =
                    std::alloc::Layout::from_size_align(capacity * element.size, element.align)
                        .unwrap();
                NonNull::new_unchecked(std::alloc::alloc(layout))
            } else {
                NonNull::new_unchecked(element.align as *mut u8)
            }
        };

        TagStorage {
            ptr,
            capacity,
            len: 0,
            element,
        }
    }

    /// Gets the element metadata.
    pub fn element(&self) -> &TagMeta {
        &self.element
    }

    /// Gets the number of tags contained within the vector.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Determines if the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len() < 1
    }

    /// Pushes a new tag onto the end of the vector.
    ///
    /// # Safety
    ///
    /// Ensure the tag pointed to by `ptr` is representative
    /// of the tag types stored in the vec.
    ///
    /// `ptr` must not point to a location already within the vector.
    ///
    /// The value at `ptr` is _copied_ into the tag vector. If the value
    /// is not `Copy`, then the caller must ensure that the original value
    /// is forgotten with `mem::forget` such that the finalizer is not called
    /// twice.
    pub unsafe fn push_raw(&mut self, ptr: *const u8) {
        if self.len == self.capacity {
            self.grow();
        }

        if self.element.size > 0 {
            let dst = self.ptr.as_ptr().add(self.len * self.element.size);
            std::ptr::copy_nonoverlapping(ptr, dst, self.element.size);
        }

        self.len += 1;
    }

    /// Pushes a new tag onto the end of the vector.
    ///
    /// # Safety
    ///
    /// Ensure that the type `T` is representative of the tag type stored in the vec.
    pub unsafe fn push<T: Tag>(&mut self, value: T) {
        debug_assert!(
            size_of::<T>() == self.element.size,
            "incompatible element data size"
        );
        self.push_raw(&value as *const T as *const u8);
        std::mem::forget(value);
    }

    /// Gets a raw pointer to the start of the tag slice.
    ///
    /// Returns a tuple containing `(pointer, element_size, count)`.
    pub unsafe fn data_raw(&self) -> (NonNull<u8>, usize, usize) {
        (self.ptr, self.element.size, self.len)
    }

    /// Gets a shared reference to the slice of tags.
    ///
    /// # Safety
    ///
    /// Ensure that `T` is representative of the tag data actually stored.
    ///
    /// Access to the tag data within the slice is runtime borrow checked.
    /// This call will panic if borrowing rules are broken.
    pub unsafe fn data_slice<T>(&self) -> &[T] {
        debug_assert!(
            size_of::<T>() == self.element.size,
            "incompatible element data size"
        );
        std::slice::from_raw_parts(self.ptr.as_ptr() as *const T, self.len)
    }

    fn grow(&mut self) {
        assert!(self.element.size != 0, "capacity overflow");
        unsafe {
            let (new_cap, ptr) = {
                let layout = std::alloc::Layout::from_size_align(
                    self.capacity * self.element.size,
                    self.element.align,
                )
                .unwrap();
                let new_cap = 2 * self.capacity;
                let ptr =
                    std::alloc::realloc(self.ptr.as_ptr(), layout, new_cap * self.element.size);

                (new_cap, ptr)
            };

            if ptr.is_null() {
                println!("out of memory");
                std::process::abort()
            }

            self.ptr = NonNull::new_unchecked(ptr);
            self.capacity = new_cap;
        }
    }
}

unsafe impl Sync for TagStorage {}

unsafe impl Send for TagStorage {}

impl Drop for TagStorage {
    fn drop(&mut self) {
        if self.element.size > 0 {
            let ptr = self.ptr.as_ptr();

            unsafe {
                if let Some(drop_fn) = self.element.drop_fn {
                    for i in 0..self.len {
                        drop_fn(ptr.add(i * self.element.size));
                    }
                }
                let layout = std::alloc::Layout::from_size_align_unchecked(
                    self.element.size * self.capacity,
                    self.element.align,
                );
                std::alloc::dealloc(ptr, layout);
            }
        }
    }
}

impl Debug for TagStorage {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "TagStorage {{ element_size: {}, count: {}, capacity: {} }}",
            self.element.size, self.len, self.capacity
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Copy, Clone, PartialEq, Debug)]
    struct ZeroSize;

    #[test]
    pub fn create() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<usize>();
        desc.register_component::<isize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|tags| unsafe {
            tags.get_mut(TagTypeId::of::<usize>()).unwrap().push(1isize)
        });

        let chunk_index = data.get_free_chunk(set);
        let components = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();
        let (chunk_entities, chunk_components) = components.write();

        chunk_entities.push(Entity::new(1, Wrapping(0)));
        unsafe {
            (&mut *chunk_components.get())
                .get_mut(ComponentTypeId::of::<isize>())
                .unwrap()
                .writer()
                .push(&[1usize]);
        }
    }

    #[test]
    pub fn create_lazy_allocated() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<usize>();
        desc.register_component::<isize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|tags| unsafe {
            tags.get_mut(TagTypeId::of::<usize>()).unwrap().push(1isize)
        });

        let chunk_index = data.get_free_chunk(set);
        let chunk = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();

        assert!(!chunk.is_allocated());

        chunk.write();

        assert!(chunk.is_allocated());
    }

    #[test]
    pub fn create_free_when_empty() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<usize>();
        desc.register_component::<isize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|tags| unsafe {
            tags.get_mut(TagTypeId::of::<usize>()).unwrap().push(1isize)
        });

        let chunk_index = data.get_free_chunk(set);
        let chunk = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();

        assert!(!chunk.is_allocated());

        let (chunk_entities, chunk_components) = chunk.write();

        chunk_entities.push(Entity::new(1, Wrapping(0)));
        unsafe {
            (&mut *chunk_components.get())
                .get_mut(ComponentTypeId::of::<isize>())
                .unwrap()
                .writer()
                .push(&[1usize]);
        }

        assert!(chunk.is_allocated());

        chunk.swap_remove(0, true);

        assert!(!chunk.is_allocated());
    }

    #[test]
    pub fn read_components() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_component::<isize>();
        desc.register_component::<usize>();
        desc.register_component::<ZeroSize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|_| {});
        let chunk_index = data.get_free_chunk(set);
        let components = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();

        let entities = [
            (Entity::new(1, Wrapping(0)), 1isize, 1usize, ZeroSize),
            (Entity::new(2, Wrapping(0)), 2isize, 2usize, ZeroSize),
            (Entity::new(3, Wrapping(0)), 3isize, 3usize, ZeroSize),
        ];

        let (chunk_entities, chunk_components) = components.write();
        for (entity, c1, c2, c3) in entities.iter() {
            chunk_entities.push(*entity);
            unsafe {
                (&mut *chunk_components.get())
                    .get_mut(ComponentTypeId::of::<isize>())
                    .unwrap()
                    .writer()
                    .push(&[*c1]);
                (&mut *chunk_components.get())
                    .get_mut(ComponentTypeId::of::<usize>())
                    .unwrap()
                    .writer()
                    .push(&[*c2]);
                (&mut *chunk_components.get())
                    .get_mut(ComponentTypeId::of::<ZeroSize>())
                    .unwrap()
                    .writer()
                    .push(&[*c3]);
            }
        }

        unsafe {
            for (i, c) in (*chunk_components.get())
                .get(ComponentTypeId::of::<isize>())
                .unwrap()
                .data_slice::<isize>()
                .iter()
                .enumerate()
            {
                assert_eq!(entities[i].1, *c);
            }

            for (i, c) in (*chunk_components.get())
                .get(ComponentTypeId::of::<usize>())
                .unwrap()
                .data_slice::<usize>()
                .iter()
                .enumerate()
            {
                assert_eq!(entities[i].2, *c);
            }

            for (i, c) in (*chunk_components.get())
                .get(ComponentTypeId::of::<ZeroSize>())
                .unwrap()
                .data_slice::<ZeroSize>()
                .iter()
                .enumerate()
            {
                assert_eq!(entities[i].3, *c);
            }
        }
    }

    #[test]
    pub fn read_tags() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<isize>();
        desc.register_tag::<ZeroSize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);

        let tag_values = [(0isize, ZeroSize), (1isize, ZeroSize), (2isize, ZeroSize)];

        for (t1, t2) in tag_values.iter() {
            data.alloc_chunk_set(|tags| {
                unsafe { tags.get_mut(TagTypeId::of::<isize>()).unwrap().push(*t1) };
                unsafe { tags.get_mut(TagTypeId::of::<ZeroSize>()).unwrap().push(*t2) };
            });
        }

        unsafe {
            let tags1 = data
                .tags()
                .get(TagTypeId::of::<isize>())
                .unwrap()
                .data_slice::<isize>();
            assert_eq!(tags1.len(), tag_values.len());
            for (i, t) in tags1.iter().enumerate() {
                assert_eq!(tag_values[i].0, *t);
            }

            let tags2 = data
                .tags()
                .get(TagTypeId::of::<ZeroSize>())
                .unwrap()
                .data_slice::<ZeroSize>();
            assert_eq!(tags2.len(), tag_values.len());
            for (i, t) in tags2.iter().enumerate() {
                assert_eq!(tag_values[i].1, *t);
            }
        }
    }

    #[test]
    pub fn create_zero_size_tags() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<ZeroSize>();
        desc.register_component::<isize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|tags| unsafe {
            tags.get_mut(TagTypeId::of::<ZeroSize>())
                .unwrap()
                .push(ZeroSize);
        });

        let chunk_index = data.get_free_chunk(set);
        let components = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();
        let (chunk_entities, chunk_components) = components.write();

        chunk_entities.push(Entity::new(1, Wrapping(0)));
        unsafe {
            (&mut *chunk_components.get())
                .get_mut(ComponentTypeId::of::<isize>())
                .unwrap()
                .writer()
                .push(&[1usize]);
        }
    }

    #[test]
    pub fn create_zero_size_components() {
        let mut archetypes = Storage::new(WorldId::default());

        let mut desc = ArchetypeDescription::default();
        desc.register_tag::<usize>();
        desc.register_component::<ZeroSize>();

        let (_arch_id, data) = archetypes.alloc_archetype(desc);
        let set = data.alloc_chunk_set(|tags| unsafe {
            tags.get_mut(TagTypeId::of::<usize>()).unwrap().push(1isize);
        });

        let chunk_index = data.get_free_chunk(set);
        let components = data
            .chunksets_mut()
            .get_mut(set)
            .unwrap()
            .get_mut(chunk_index)
            .unwrap();
        let (chunk_entities, chunk_components) = components.write();

        chunk_entities.push(Entity::new(1, Wrapping(0)));
        unsafe {
            (&mut *chunk_components.get())
                .get_mut(ComponentTypeId::of::<ZeroSize>())
                .unwrap()
                .writer()
                .push(&[ZeroSize]);
        }
    }
}
