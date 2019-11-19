use crate::borrow::Exclusive;
use crate::borrow::Ref;
use crate::borrow::RefMut;
use crate::borrow::Shared;
use crate::entity::BlockAllocator;
use crate::entity::Entity;
use crate::entity::EntityAllocator;
use crate::entity::EntityLocation;
use crate::filter::ArchetypeFilterData;
use crate::filter::ChunksetFilterData;
use crate::filter::Filter;
use crate::storage::ArchetypeData;
use crate::storage::ArchetypeDescription;
use crate::storage::Component;
use crate::storage::ComponentMeta;
use crate::storage::ComponentStorage;
use crate::storage::ComponentTypeId;
use crate::storage::SliceVecIter;
use crate::storage::Storage;
use crate::storage::Tag;
use crate::storage::TagMeta;
use crate::storage::TagTypeId;
use crate::storage::Tags;
use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::iter::Enumerate;
use std::iter::Peekable;
use std::iter::Repeat;
use std::iter::Take;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// The `Universe` is a factory for creating `World`s.
///
/// Entities inserted into worlds created within the same universe are guarenteed to have
/// unique `Entity` IDs, even across worlds.
#[derive(Debug)]
pub struct Universe {
    allocator: Arc<Mutex<BlockAllocator>>,
    world_count: AtomicUsize,
}

impl Universe {
    /// Creates a new `Universe`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new `World` within this `Unvierse`.
    ///
    /// Entities inserted into worlds created within the same universe are guarenteed to have
    /// unique `Entity` IDs, even across worlds.
    pub fn create_world(&self) -> World {
        let id = self.world_count.fetch_add(1, Ordering::SeqCst);
        World::new(WorldId(id), EntityAllocator::new(self.allocator.clone()))
    }
}

impl Default for Universe {
    fn default() -> Self {
        Self {
            world_count: AtomicUsize::from(0),
            allocator: Arc::new(Mutex::new(BlockAllocator::new())),
        }
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct WorldId(usize);

/// Contains queryable collections of data associated with `Entity`s.
pub struct World {
    id: WorldId,
    storage: UnsafeCell<Storage>,
    entity_allocator: EntityAllocator,
    defrag_progress: usize,
}

unsafe impl Send for World {}

unsafe impl Sync for World {}

impl World {
    fn new(id: WorldId, allocator: EntityAllocator) -> Self {
        Self {
            id,
            storage: UnsafeCell::new(Storage::new(id)),
            entity_allocator: allocator,
            defrag_progress: 0,
        }
    }

    pub(crate) fn storage(&self) -> &Storage {
        unsafe { &*self.storage.get() }
    }

    pub(crate) fn storage_mut(&mut self) -> &mut Storage {
        unsafe { &mut *self.storage.get() }
    }

    /// Gets the unique ID of this world.
    pub fn id(&self) -> WorldId {
        self.id
    }

    /// Inserts new entities into the world.
    ///
    /// # Examples
    ///
    /// Inserting entity tuples:
    ///
    /// ```
    /// # use legion::prelude::*;
    /// # #[derive(Copy, Clone, Debug, PartialEq)]
    /// # struct Position(f32);
    /// # #[derive(Copy, Clone, Debug, PartialEq)]
    /// # struct Rotation(f32);
    /// # let universe = Universe::new();
    /// # let mut world = universe.create_world();
    /// # let model = 0u8;
    /// # let color = 0u16;
    /// let tags = (model, color);
    /// let data = vec![
    ///     (Position(0.0), Rotation(0.0)),
    ///     (Position(1.0), Rotation(1.0)),
    ///     (Position(2.0), Rotation(2.0)),
    /// ];
    /// world.insert(tags, data);
    /// ```
    pub fn insert<T, C>(&mut self, mut tags: T, components: C) -> &[Entity]
    where
        T: TagSet + TagLayout + for<'a> Filter<ChunksetFilterData<'a>>,
        C: IntoComponentSource,
    {
        // find or create archetype
        let mut components = components.into();
        let archetype_index = self.find_or_create_archetype(&mut tags, &mut components);

        // find or create chunk set
        let chunk_set_index = self.find_or_create_chunk(archetype_index, &mut tags);

        self.entity_allocator.clear_allocation_buffer();

        // insert components into chunks
        while !components.is_empty() {
            // get chunk component storage
            let archetype = unsafe {
                (&mut *self.storage.get())
                    .archetypes_mut()
                    .get_unchecked_mut(archetype_index)
            };
            let chunk_index = archetype.get_free_chunk(chunk_set_index);
            let chunk = unsafe {
                archetype
                    .chunksets_mut()
                    .get_unchecked_mut(chunk_set_index)
                    .get_unchecked_mut(chunk_index)
            };

            // insert as many components as we can into the chunk
            let allocated = components.write(&mut self.entity_allocator, chunk);

            // record new entity locations
            let start = chunk.len() - allocated;
            let added = chunk.entities().iter().enumerate().skip(start);
            for (i, e) in added {
                let location =
                    EntityLocation::new(archetype_index, chunk_set_index, chunk_index, i);
                self.entity_allocator.set_location(e.index(), location);
            }
        }

        self.entity_allocator.allocation_buffer()
    }

    /// Removes the given `Entity` from the `World`.
    ///
    /// Returns `true` if the entity was deleted; else `false`.
    pub fn delete(&mut self, entity: Entity) -> bool {
        if let Some(location) = self.entity_allocator.delete_entity(entity) {
            // find entity's chunk
            let chunk = self
                .storage_mut()
                .archetypes_mut()
                .get_mut(location.archetype())
                .unwrap()
                .chunksets_mut()
                .get_mut(location.set())
                .unwrap()
                .get_mut(location.chunk())
                .unwrap();

            // swap remove with last entity in chunk
            if let Some(swapped) = chunk.swap_remove(location.component(), true) {
                // record swapped entity's new location
                self.entity_allocator
                    .set_location(swapped.index(), location);
            }

            true
        } else {
            false
        }
    }

    fn find_chunk_with_delta(
        &mut self,
        source_location: EntityLocation,
        add_components: &[(ComponentTypeId, ComponentMeta)],
        remove_components: &[ComponentTypeId],
        add_tags: &[(TagTypeId, TagMeta, NonNull<u8>)],
        remove_tags: &[TagTypeId],
    ) -> (usize, usize) {
        let archetype = {
            let result = {
                let source_archetype = self
                    .storage()
                    .archetypes()
                    .get(source_location.archetype())
                    .unwrap();

                // find target chunk
                let mut component_layout = DynamicComponentLayout {
                    existing: source_archetype.description().components(),
                    add: add_components,
                    remove: remove_components,
                };

                let mut tag_layout = DynamicTagLayout {
                    storage: self.storage(),
                    archetype: source_location.archetype(),
                    chunk: source_location.chunk(),
                    existing: source_archetype.description().tags(),
                    add: add_tags,
                    remove: remove_tags,
                };

                let archetype = self.find_archetype(&mut tag_layout, &mut component_layout);
                if let Some(archetype) = archetype.as_ref() {
                    if let Some(chunk) = self.find_chunk_set(*archetype, &mut tag_layout) {
                        // fast path: chunk already exists
                        return (*archetype, chunk);
                    }

                    Ok(*archetype)
                } else {
                    let mut description = ArchetypeDescription::default();
                    component_layout.tailor_archetype(&mut description);
                    tag_layout.tailor_archetype(&mut description);

                    Err(description)
                }
            };

            match result {
                Ok(arch) => arch,
                Err(desc) => {
                    let (index, _) = self.storage_mut().alloc_archetype(desc);
                    index
                }
            }
        };

        // slow path: create new chunk
        let source_archetype = self
            .storage()
            .archetypes()
            .get(source_location.archetype())
            .unwrap();
        let mut tags = source_archetype.tags().tag_set(source_location.chunk());
        for type_id in remove_tags.iter() {
            tags.remove(*type_id);
        }
        for (type_id, meta, ptr) in add_tags.iter() {
            tags.push(*type_id, *meta, *ptr);
        }

        let chunk = self.create_chunk_set(archetype, &tags);

        (archetype, chunk)
    }

    fn move_entity(
        &mut self,
        entity: Entity,
        add_components: &[(ComponentTypeId, ComponentMeta)],
        remove_components: &[ComponentTypeId],
        add_tags: &[(TagTypeId, TagMeta, NonNull<u8>)],
        remove_tags: &[TagTypeId],
    ) -> &mut ComponentStorage {
        let location = self
            .entity_allocator
            .get_location(entity.index())
            .expect("entity not found");

        // find or create the target chunk
        let (target_arch_index, target_chunkset_index) = self.find_chunk_with_delta(
            location,
            add_components,
            remove_components,
            add_tags,
            remove_tags,
        );

        // Safety Note:
        // It is only safe for us to have 2 &mut references to storage here because
        // we know we are only going to be modifying two chunks that are at different
        // indexes.

        // fetch entity's chunk
        let current_chunk = unsafe { &mut *self.storage.get() }
            .archetypes_mut()
            .get_mut(location.archetype())
            .unwrap()
            .chunksets_mut()
            .get_mut(location.set())
            .unwrap()
            .get_mut(location.chunk())
            .unwrap();

        // fetch target chunk
        let archetype = unsafe { &mut *self.storage.get() }
            .archetypes_mut()
            .get_mut(target_arch_index)
            .unwrap();
        let target_chunk_index = archetype.get_free_chunk(target_chunkset_index);
        let target_chunk = unsafe {
            archetype
                .chunksets_mut()
                .get_unchecked_mut(target_chunkset_index)
                .get_unchecked_mut(target_chunk_index)
        };

        // move existing data over into new chunk
        if let Some(swapped) = current_chunk.move_entity(target_chunk, location.component()) {
            // update location of any entity that was moved into the previous location
            self.entity_allocator
                .set_location(swapped.index(), location);
        }

        // record the entity's new location
        self.entity_allocator.set_location(
            entity.index(),
            EntityLocation::new(
                target_arch_index,
                target_chunkset_index,
                target_chunk_index,
                target_chunk.len() - 1,
            ),
        );

        target_chunk
    }

    /// Adds a component to an entity, or set's its value if the component is
    /// already present.
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        if let Some(mut comp) = self.get_component_mut(entity) {
            *comp = component;
            return;
        }

        // move the entity into a suitable chunk
        let target_chunk = self.move_entity(
            entity,
            &[(ComponentTypeId::of::<T>(), ComponentMeta::of::<T>())],
            &[],
            &[],
            &[],
        );

        // push new component into chunk
        let (_, components) = target_chunk.write();
        unsafe {
            let components = &mut *components.get();
            components
                .get_mut(ComponentTypeId::of::<T>())
                .unwrap()
                .writer()
                .push(&[component]);
        }
    }

    /// Removes a component from an entity.
    pub fn remove_component<T: Component>(&mut self, entity: Entity) {
        if self.get_component::<T>(entity).is_some() {
            // move the entity into a suitable chunk
            self.move_entity(entity, &[], &[ComponentTypeId::of::<T>()], &[], &[]);
        }
    }

    /// Adds a tag to an entity, or set's its value if the tag is
    /// already present.
    pub fn add_tag<T: Tag>(&mut self, entity: Entity, tag: T) {
        if self.get_tag::<T>(entity).is_some() {
            self.remove_tag::<T>(entity);
        }

        // move the entity into a suitable chunk
        self.move_entity(
            entity,
            &[],
            &[],
            &[(
                TagTypeId::of::<T>(),
                TagMeta::of::<T>(),
                NonNull::new(&tag as *const _ as *mut u8).unwrap(),
            )],
            &[],
        );
    }

    /// Removes a tag from an entity.
    pub fn remove_tag<T: Tag>(&mut self, entity: Entity) {
        if self.get_tag::<T>(entity).is_some() {
            // move the entity into a suitable chunk
            self.move_entity(entity, &[], &[], &[], &[TagTypeId::of::<T>()]);
        }
    }

    /// Borrows component data for the given entity.
    ///
    /// Returns `Some(data)` if the entity was found and contains the specified data.
    /// Otherwise `None` is returned.
    ///
    /// # Panics
    ///
    /// This function borrows all components of type `T` in the world. It may panic if
    /// any other code is currently borrowing `T` mutably (such as in a query).
    pub fn get_component<T: Component>(&self, entity: Entity) -> Option<Ref<Shared, T>> {
        if !self.is_alive(entity) {
            return None;
        }

        let location = self.entity_allocator.get_location(entity.index())?;
        let archetype = self.storage().archetypes().get(location.archetype())?;
        let chunk = archetype
            .chunksets()
            .get(location.set())?
            .get(location.chunk())?;
        let (slice_borrow, slice) = unsafe {
            chunk
                .components(ComponentTypeId::of::<T>())?
                .data_slice::<T>()
                .deconstruct()
        };
        let component = slice.get(location.component())?;

        Some(Ref::new(slice_borrow, component))
    }

    /// Mutably borrows entity data for the given entity.
    ///
    /// Returns `Some(data)` if the entity was found and contains the specified data.
    /// Otherwise `None` is returned.
    ///
    /// # Panics
    ///
    /// This function borrows all components of type `T` in the world. It may panic if
    /// any other code is currently borrowing `T` (such as in a query).
    pub fn get_component_mut<T: Component>(&self, entity: Entity) -> Option<RefMut<Exclusive, T>> {
        if !self.is_alive(entity) {
            return None;
        }

        let location = self.entity_allocator.get_location(entity.index())?;
        let archetype = self.storage().archetypes().get(location.archetype())?;
        let chunk = archetype
            .chunksets()
            .get(location.set())?
            .get(location.chunk())?;
        let (slice_borrow, slice) = unsafe {
            chunk
                .components(ComponentTypeId::of::<T>())?
                .data_slice_mut::<T>()
                .deconstruct()
        };
        let component = slice.get_mut(location.component())?;

        Some(RefMut::new(slice_borrow, component))
    }

    pub fn get_component_changed<T: Component>(
        &self,
        entity: Entity,
        mark_unchanged: bool,
    ) -> Option<Ref<Shared, T>> {
        if !self.is_alive(entity) {
            return None;
        }

        let location = self.entity_allocator.get_location(entity.index())?;
        let archetype = self.storage().archetypes().get(location.archetype())?;
        let chunk = archetype
            .chunksets()
            .get(location.set())?
            .get(location.chunk())?;

        let component_accessor = chunk.components(ComponentTypeId::of::<T>())?;

        if component_accessor.changed() {
            let (slice_borrow, slice) =
                unsafe { component_accessor.data_slice::<T>().deconstruct() };
            let component = slice.get(location.component())?;

            if mark_unchanged {
                component_accessor.mark_unchanged();
            }
            Some(Ref::new(slice_borrow, component))
        } else {
            None
        }
    }
    pub fn get_component_changed_mut<T: Component>(
        &self,
        entity: Entity,
        mark_unchanged: bool,
    ) -> Option<RefMut<Exclusive, T>> {
        if !self.is_alive(entity) {
            return None;
        }

        let location = self.entity_allocator.get_location(entity.index())?;
        let archetype = self.storage().archetypes().get(location.archetype())?;
        let chunk = archetype
            .chunksets()
            .get(location.set())?
            .get(location.chunk())?;

        let component_accessor = chunk.components(ComponentTypeId::of::<T>())?;

        if component_accessor.changed() {
            let (slice_borrow, slice) =
                unsafe { component_accessor.data_slice_mut::<T>().deconstruct() };
            let component = slice.get_mut(location.component())?;

            if mark_unchanged {
                component_accessor.mark_unchanged();
            }
            Some(RefMut::new(slice_borrow, component))
        } else {
            None
        }
    }

    /// Gets tag data for the given entity.
    ///
    /// Returns `Some(data)` if the entity was found and contains the specified data.
    /// Otherwise `None` is returned.
    pub fn get_tag<T: Tag>(&self, entity: Entity) -> Option<&T> {
        if !self.is_alive(entity) {
            return None;
        }

        let location = self.entity_allocator.get_location(entity.index())?;
        let archetype = self.storage().archetypes().get(location.archetype())?;
        let tags = archetype.tags().get(TagTypeId::of::<T>())?;
        unsafe { tags.data_slice::<T>().get(location.set()) }
    }

    /// Determines if the given `Entity` is alive within this `World`.
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entity_allocator.is_alive(entity)
    }

    /// Iteratively defragments the world's internal memory.
    ///
    /// This compacts entities into fewer more continuous chunks.
    ///
    /// `budget` describes the maximum number of entities that can be moved
    /// in one call. Subsequent calls to `defrag` will resume progress from the
    /// previous call.
    pub fn defrag(&mut self, budget: Option<usize>) {
        let archetypes = unsafe { &mut *self.storage.get() }.archetypes_mut();
        let mut budget = budget.unwrap_or(std::usize::MAX);
        let start = self.defrag_progress;
        while self.defrag_progress < archetypes.len() {
            // defragment the next archetype
            let complete =
                (&mut archetypes[self.defrag_progress]).defrag(&mut budget, |e, location| {
                    self.entity_allocator.set_location(e.index(), location);
                });
            if complete {
                // increment the index, looping it once we get to the end
                self.defrag_progress = (self.defrag_progress + 1) % archetypes.len();
            }

            // stop once we run out of budget or reach back to where we started
            if budget == 0 || self.defrag_progress == start {
                break;
            }
        }
    }

    pub fn merge(&mut self, world: World) {
        self.entity_allocator.merge(world.entity_allocator);

        for archetype in unsafe { &mut *world.storage.get() }.drain(..) {
            // use the description as an archetype filter
            let mut desc = archetype.description().clone();
            let archetype_data = ArchetypeFilterData {
                component_types: self.storage().component_types(),
                tag_types: self.storage().tag_types(),
            };
            let matches = desc.matches(archetype_data).matching_indices().next();
            if let Some(arch_index) = matches {
                // similar archetype already exists, merge
                self.storage_mut()
                    .archetypes_mut()
                    .get_mut(arch_index)
                    .unwrap()
                    .merge(archetype);
            } else {
                // archetype does not already exist, append
                self.storage_mut().push(archetype);
            }
        }
    }

    fn find_archetype<T, C>(&self, tags: &mut T, components: &mut C) -> Option<usize>
    where
        T: for<'a> Filter<ArchetypeFilterData<'a>>,
        C: for<'a> Filter<ArchetypeFilterData<'a>>,
    {
        // search for an archetype with an exact match for the desired component layout
        let archetype_data = ArchetypeFilterData {
            component_types: self.storage().component_types(),
            tag_types: self.storage().tag_types(),
        };

        // zip the two filters together - find the first index that matches both
        tags.matches(archetype_data)
            .zip(components.matches(archetype_data))
            .enumerate()
            .take(self.storage().archetypes().len())
            .filter(|(_, (a, b))| *a && *b)
            .map(|(i, _)| i)
            .next()
    }

    fn create_archetype<T, C>(&mut self, tags: &T, components: &C) -> usize
    where
        T: TagLayout,
        C: ComponentLayout,
    {
        let mut description = ArchetypeDescription::default();
        tags.tailor_archetype(&mut description);
        components.tailor_archetype(&mut description);

        let (index, _) = self.storage_mut().alloc_archetype(description);
        index
    }

    fn find_or_create_archetype<T, C>(&mut self, tags: &mut T, components: &mut C) -> usize
    where
        T: TagLayout,
        C: ComponentLayout,
    {
        if let Some(i) = self.find_archetype(tags.get_filter(), components.get_filter()) {
            i
        } else {
            self.create_archetype(tags, components)
        }
    }

    fn find_chunk_set<T>(&self, archetype: usize, tags: &mut T) -> Option<usize>
    where
        T: for<'a> Filter<ChunksetFilterData<'a>>,
    {
        // fetch the archetype, we can already assume that the archetype index is valid
        let archetype_data = unsafe { self.storage().archetypes().get_unchecked(archetype) };

        // find a chunk with the correct tags
        let chunk_filter_data = ChunksetFilterData {
            archetype_data: archetype_data.deref(),
        };

        if let Some(i) = tags.matches(chunk_filter_data).matching_indices().next() {
            return Some(i);
        }

        None
    }

    fn create_chunk_set<T>(&mut self, archetype: usize, tags: &T) -> usize
    where
        T: TagSet,
    {
        let archetype_data = unsafe {
            self.storage_mut()
                .archetypes_mut()
                .get_unchecked_mut(archetype)
        };
        archetype_data.alloc_chunk_set(|chunk_tags| tags.write_tags(chunk_tags))
    }

    fn find_or_create_chunk<T>(&mut self, archetype: usize, tags: &mut T) -> usize
    where
        T: TagSet + for<'a> Filter<ChunksetFilterData<'a>>,
    {
        if let Some(i) = self.find_chunk_set(archetype, tags) {
            i
        } else {
            self.create_chunk_set(archetype, tags)
        }
    }
}

/// Describes the types of a set of components attached to an entity.
pub trait ComponentLayout: Sized {
    /// A filter type which filters archetypes to an exact match with this layout.
    type Filter: for<'a> Filter<ArchetypeFilterData<'a>>;

    /// Gets the archetype filter for this layout.
    fn get_filter(&mut self) -> &mut Self::Filter;

    /// Modifies an archetype description to include the components described by this layout.
    fn tailor_archetype(&self, archetype: &mut ArchetypeDescription);
}

/// Describes the types of a set of tags attached to an entity.
pub trait TagLayout: Sized {
    /// A filter type which filters archetypes to an exact match with this layout.
    type Filter: for<'a> Filter<ArchetypeFilterData<'a>>;

    /// Gets the archetype filter for this layout.
    fn get_filter(&mut self) -> &mut Self::Filter;

    /// Modifies an archetype description to include the tags described by this layout.
    fn tailor_archetype(&self, archetype: &mut ArchetypeDescription);
}

/// A set of tag values to be attached to an entity.
pub trait TagSet {
    /// Writes the tags in this set to a new chunk.
    fn write_tags(&self, tags: &mut Tags);
}

/// A set of components to be attached to one or more entities.
pub trait ComponentSource: ComponentLayout {
    /// Determines if this component source has any more entity data to write.
    fn is_empty(&mut self) -> bool;

    /// Writes as many components as possible into a chunk.
    fn write(&mut self, allocator: &mut EntityAllocator, chunk: &mut ComponentStorage) -> usize;
}

/// An object that can be converted into a `ComponentSource`.
pub trait IntoComponentSource {
    /// The component source type that can be converted into.
    type Source: ComponentSource;

    /// Converts `self` into a component source.
    fn into(self) -> Self::Source;
}

/// A `ComponentSource` which can insert tuples of components representing each entity into a world.
pub struct ComponentTupleSet<T, I>
where
    I: Iterator<Item = T>,
{
    iter: Peekable<I>,
    filter: ComponentTupleFilter<T>,
}

impl<T, I> From<I> for ComponentTupleSet<T, I>
where
    I: Iterator<Item = T>,
    ComponentTupleSet<T, I>: ComponentSource,
{
    fn from(iter: I) -> Self {
        ComponentTupleSet {
            iter: iter.peekable(),
            filter: ComponentTupleFilter {
                _phantom: PhantomData,
            },
        }
    }
}

impl<I> IntoComponentSource for I
where
    I: IntoIterator,
    ComponentTupleSet<I::Item, I::IntoIter>: ComponentSource,
{
    type Source = ComponentTupleSet<I::Item, I::IntoIter>;

    fn into(self) -> Self::Source {
        ComponentTupleSet {
            iter: self.into_iter().peekable(),
            filter: ComponentTupleFilter {
                _phantom: PhantomData,
            },
        }
    }
}

pub struct ComponentTupleFilter<T> {
    _phantom: PhantomData<T>,
}

mod tuple_impls {
    use super::*;
    use crate::storage::Component;
    use crate::storage::ComponentTypeId;
    use crate::storage::SliceVecIter;
    use crate::storage::Tag;
    use itertools::Zip;
    use std::iter::Repeat;
    use std::iter::Take;
    use std::slice::Iter;

    macro_rules! impl_data_tuple {
        ( $( $ty: ident => $id: ident ),* ) => {
            impl_data_tuple!(@TAG_SET $( $ty => $id ),*);
            impl_data_tuple!(@COMPONENT_SOURCE $( $ty => $id ),*);
        };
        ( @COMPONENT_SOURCE $( $ty: ident => $id: ident ),* ) => {
            impl<I, $( $ty ),*> ComponentLayout for ComponentTupleSet<($( $ty, )*), I>
            where
                I: Iterator<Item = ($( $ty, )*)>,
                $( $ty: Component ),*
            {
                type Filter = ComponentTupleFilter<($( $ty, )*)>;

                fn get_filter(&mut self) -> &mut Self::Filter {
                    &mut self.filter
                }

                fn tailor_archetype(&self, archetype: &mut ArchetypeDescription) {
                    #![allow(unused_variables)]
                    $(
                        archetype.register_component::<$ty>();
                    )*
                }
            }

            impl<I, $( $ty ),*> ComponentSource for ComponentTupleSet<($( $ty, )*), I>
            where
                I: Iterator<Item = ($( $ty, )*)>,
                $( $ty: Component ),*
            {
                fn is_empty(&mut self) -> bool {
                    self.iter.peek().is_none()
                }

                fn write(&mut self, allocator: &mut EntityAllocator, chunk: &mut ComponentStorage) -> usize {
                    #![allow(unused_variables)]
                    #![allow(unused_unsafe)]
                    #![allow(non_snake_case)]
                    let space = chunk.capacity() - chunk.len();
                    let (entities, components) = chunk.write();
                    let mut count = 0;

                    unsafe {
                        $(
                            let mut $ty = (&mut *components.get()).get_mut(ComponentTypeId::of::<$ty>()).unwrap().writer();
                        )*

                        while let Some(($( $id, )*)) = { if count == space { None } else { self.iter.next() } } {
                            let entity = allocator.create_entity();
                            entities.push(entity);
                            $(
                                let slice = [$id];
                                $ty.push(&slice);
                                std::mem::forget(slice);
                            )*
                            count += 1;
                        }
                    }

                    count
                }
            }

            impl<'a, $( $ty ),*> Filter<ArchetypeFilterData<'a>> for ComponentTupleFilter<($( $ty, )*)>
            where
                $( $ty: Component ),*
            {
                type Iter = SliceVecIter<'a, ComponentTypeId>;

                fn collect(&self, source: ArchetypeFilterData<'a>) -> Self::Iter {
                    source.component_types.iter()
                }

                fn is_match(&mut self, item: &<Self::Iter as Iterator>::Item) -> Option<bool> {
                    let types = &[$( ComponentTypeId::of::<$ty>() ),*];
                    Some(types.len() == item.len() && types.iter().all(|t| item.contains(t)))
                }
            }
        };
        ( @TAG_SET $( $ty: ident => $id: ident ),* ) => {
            impl_data_tuple!(@CHUNK_FILTER $( $ty => $id ),*);

            impl<$( $ty ),*> TagSet for ($( $ty, )*)
            where
                $( $ty: Tag ),*
            {
                fn write_tags(&self, tags: &mut Tags) {
                    #![allow(unused_variables)]
                    #![allow(non_snake_case)]
                    let ($($id,)*) = self;
                    $(
                        unsafe {
                            tags.get_mut(TagTypeId::of::<$ty>())
                                .unwrap()
                                .push($id.clone())
                        };
                    )*
                }
            }

            impl <$( $ty ),*> TagLayout for ($( $ty, )*)
            where
                $( $ty: Tag ),*
            {
                type Filter = Self;

                fn get_filter(&mut self) -> &mut Self {
                    self
                }

                fn tailor_archetype(&self, archetype: &mut ArchetypeDescription) {
                    #![allow(unused_variables)]
                    $(
                        archetype.register_tag::<$ty>();
                    )*
                }
            }

            impl<'a, $( $ty ),*> Filter<ArchetypeFilterData<'a>> for ($( $ty, )*)
            where
                $( $ty: Tag ),*
            {
                type Iter = SliceVecIter<'a, TagTypeId>;

                fn collect(&self, source: ArchetypeFilterData<'a>) -> Self::Iter {
                    source.tag_types.iter()
                }

                fn is_match(&mut self, item: &<Self::Iter as Iterator>::Item) -> Option<bool> {
                    let types = &[$( TagTypeId::of::<$ty>() ),*];
                    Some(types.len() == item.len() && types.iter().all(|t| item.contains(t)))
                }
            }
        };
        ( @CHUNK_FILTER $( $ty: ident => $id: ident ),+ ) => {
            impl<'a, $( $ty ),*> Filter<ChunksetFilterData<'a>> for ($( $ty, )*)
            where
                $( $ty: Tag ),*
            {
                type Iter = Zip<($( Iter<'a, $ty>, )*)>;

                fn collect(&self, source: ChunksetFilterData<'a>) -> Self::Iter {
                    let iters = (
                        $(
                            unsafe {
                                source.archetype_data
                                    .tags()
                                    .get(TagTypeId::of::<$ty>())
                                    .unwrap()
                                    .data_slice::<$ty>()
                                    .iter()
                            },
                        )*

                    );

                    itertools::multizip(iters)
                }

                fn is_match(&mut self, item: &<Self::Iter as Iterator>::Item) -> Option<bool> {
                    #![allow(non_snake_case)]
                    let ($( $ty, )*) = self;
                    Some(($( &*$ty, )*) == *item)
                }
            }
        };
        ( @CHUNK_FILTER ) => {
            impl<'a> Filter<ChunksetFilterData<'a>> for () {
                type Iter = Take<Repeat<()>>;

                fn collect(&self, source: ChunksetFilterData<'a>) -> Self::Iter {
                    std::iter::repeat(()).take(source.archetype_data.len())
                }

                fn is_match(&mut self, _: &<Self::Iter as Iterator>::Item) -> Option<bool> {
                    Some(true)
                }
            }
        };
    }

    impl_data_tuple!();
    impl_data_tuple!(A => a);
    impl_data_tuple!(A => a, B => b);
    impl_data_tuple!(A => a, B => b, C => c);
    impl_data_tuple!(A => a, B => b, C => c, D => d);
    impl_data_tuple!(A => a, B => b, C => c, D => d, E => e);
}

struct DynamicComponentLayout<'a> {
    existing: &'a [(ComponentTypeId, ComponentMeta)],
    add: &'a [(ComponentTypeId, ComponentMeta)],
    remove: &'a [ComponentTypeId],
}

impl<'a> ComponentLayout for DynamicComponentLayout<'a> {
    type Filter = Self;

    fn get_filter(&mut self) -> &mut Self::Filter {
        self
    }

    fn tailor_archetype(&self, archetype: &mut ArchetypeDescription) {
        // copy components from existing archetype into new
        // except for those in `remove`
        let components = self
            .existing
            .iter()
            .filter(|(t, _)| !self.remove.contains(t));

        for (comp_type, meta) in components {
            archetype.register_component_raw(*comp_type, *meta);
        }

        // append components from `add`
        for (comp_type, meta) in self.add.iter() {
            archetype.register_component_raw(*comp_type, *meta);
        }
    }
}

impl<'a, 'b> Filter<ArchetypeFilterData<'b>> for DynamicComponentLayout<'a> {
    type Iter = SliceVecIter<'b, ComponentTypeId>;

    fn collect(&self, source: ArchetypeFilterData<'b>) -> Self::Iter {
        source.component_types.iter()
    }

    fn is_match(&mut self, item: &<Self::Iter as Iterator>::Item) -> Option<bool> {
        Some(
            item.len() == (self.existing.len() + self.add.len() - self.remove.len())
                && item.iter().all(|t| {
                    // all types are not in remove
                    !self.remove.contains(t)
                    // any are either in existing or add
                        && (self.existing.iter().any(|(x, _)| x == t)
                            || self.add.iter().any(|(x, _)| x == t))
                }),
        )
    }
}

struct DynamicTagLayout<'a> {
    storage: &'a Storage,
    archetype: usize,
    chunk: usize,
    existing: &'a [(TagTypeId, TagMeta)],
    add: &'a [(TagTypeId, TagMeta, NonNull<u8>)],
    remove: &'a [TagTypeId],
}

unsafe impl<'a> Send for DynamicTagLayout<'a> {}

unsafe impl<'a> Sync for DynamicTagLayout<'a> {}

impl<'a> TagLayout for DynamicTagLayout<'a> {
    type Filter = Self;

    fn get_filter(&mut self) -> &mut Self::Filter {
        self
    }

    fn tailor_archetype(&self, archetype: &mut ArchetypeDescription) {
        // copy tags from existing archetype into new
        // except for those in `remove`
        let tags = self
            .existing
            .iter()
            .filter(|(t, _)| !self.remove.contains(t));

        for (tag_type, meta) in tags {
            archetype.register_tag_raw(*tag_type, *meta);
        }

        // append tag from `add`
        for (tag_type, meta, _) in self.add.iter() {
            archetype.register_tag_raw(*tag_type, *meta);
        }
    }
}

impl<'a, 'b> Filter<ArchetypeFilterData<'b>> for DynamicTagLayout<'a> {
    type Iter = SliceVecIter<'b, TagTypeId>;

    fn collect(&self, source: ArchetypeFilterData<'b>) -> Self::Iter {
        source.tag_types.iter()
    }

    fn is_match(&mut self, item: &<Self::Iter as Iterator>::Item) -> Option<bool> {
        Some(
            item.len() == (self.existing.len() + self.add.len() - self.remove.len())
                && item.iter().all(|t| {
                    // all types are not in remove
                    !self.remove.contains(t)
                    // any are either in existing or add
                        && (self.existing.iter().any(|(x, _)| x == t)
                            || self.add.iter().any(|(x, _, _)| x == t))
                }),
        )
    }
}

impl<'a, 'b> Filter<ChunksetFilterData<'b>> for DynamicTagLayout<'a> {
    type Iter = Take<Enumerate<Repeat<&'b ArchetypeData>>>;

    fn collect(&self, source: ChunksetFilterData<'b>) -> Self::Iter {
        std::iter::repeat(source.archetype_data)
            .enumerate()
            .take(source.archetype_data.len())
    }

    fn is_match(&mut self, (chunk_index, arch): &<Self::Iter as Iterator>::Item) -> Option<bool> {
        for (type_id, meta) in self.existing {
            if self.remove.contains(type_id) {
                continue;
            }

            unsafe {
                // find the value of the tag in the source chunk
                let (slice_ptr, element_size, _) = self
                    .storage
                    .archetypes()
                    .get(self.archetype)
                    .unwrap()
                    .tags()
                    .get(*type_id)
                    .unwrap()
                    .data_raw();
                let current = slice_ptr.as_ptr().add(self.chunk * element_size);

                // find the value of the tag in the candidate chunk
                let (slice_ptr, element_size, _) = arch.tags().get(*type_id).unwrap().data_raw();
                let candidate = slice_ptr.as_ptr().add(chunk_index * element_size);

                if !meta.equals(current, candidate) {
                    return Some(false);
                }
            }
        }

        for (type_id, meta, ptr) in self.add {
            unsafe {
                let (slice_ptr, element_size, _) = arch.tags().get(*type_id).unwrap().data_raw();
                let candidate = slice_ptr.as_ptr().add(chunk_index * element_size);

                if !meta.equals(ptr.as_ptr(), candidate) {
                    return Some(false);
                }
            }
        }

        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Pos(f32, f32, f32);
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Rot(f32, f32, f32);
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Scale(f32, f32, f32);
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Vel(f32, f32, f32);
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Accel(f32, f32, f32);
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    struct Model(u32);
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    struct Static;

    fn create() -> World {
        let universe = Universe::new();
        universe.create_world()
    }

    #[test]
    fn create_universe() {
        Universe::default();
    }

    #[test]
    fn create_world() {
        let universe = Universe::new();
        universe.create_world();
    }

    #[test]
    fn insert() {
        let mut world = create();

        let shared = (1usize, 2f32, 3u16);
        let components = vec![(4f32, 5u64, 6u16), (4f32, 5u64, 6u16)];
        world.insert(shared, components);

        assert_eq!(2, world.entity_allocator.allocation_buffer().len());
    }

    #[test]
    fn get_component() {
        let mut world = create();

        let shared = (Static, Model(5));
        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        world.insert(shared, components.clone());

        for (i, e) in world
            .entity_allocator
            .allocation_buffer()
            .to_vec()
            .iter()
            .enumerate()
        {
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i).map(|(x, _)| x), Some(&x as &Pos)),
                None => assert_eq!(components.get(i).map(|(x, _)| x), None),
            }
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i).map(|(_, x)| x), Some(&x as &Rot)),
                None => assert_eq!(components.get(i).map(|(_, x)| x), None),
            }
        }
    }

    #[test]
    fn get_component_wrong_type() {
        let mut world = create();

        world.insert((), vec![(0f64,)]);

        let entity = *world.entity_allocator.allocation_buffer().get(0).unwrap();

        assert!(world.get_component::<i32>(entity).is_none());
    }

    #[test]
    fn get_tag() {
        let mut world = create();

        let shared = (Static, Model(5));
        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        world.insert(shared, components);

        for e in world.entity_allocator.allocation_buffer().to_vec().iter() {
            assert_eq!(&Static, world.get_tag::<Static>(*e).unwrap().deref());
            assert_eq!(&Model(5), world.get_tag::<Model>(*e).unwrap().deref());
        }
    }

    #[test]
    fn get_tag_wrong_type() {
        let mut world = create();

        world.insert((Static,), vec![(0f64,)]);

        let entity = *world.entity_allocator.allocation_buffer().get(0).unwrap();

        assert!(world.get_tag::<Model>(entity).is_none());
    }

    #[test]
    fn delete() {
        let mut world = create();

        let shared = (Static, Model(5));
        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert(shared, components).to_vec();

        for e in entities.iter() {
            assert!(world.get_component::<Pos>(*e).is_some());
        }

        for e in entities.iter() {
            world.delete(*e);
            assert!(world.get_component::<Pos>(*e).is_none());
        }
    }

    #[test]
    fn delete_last() {
        let mut world = create();

        let shared = (Static, Model(5));
        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert(shared, components.clone()).to_vec();

        let last = *entities.last().unwrap();
        world.delete(last);

        for (i, e) in entities.iter().take(entities.len() - 1).enumerate() {
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i).map(|(x, _)| x), Some(&x as &Pos)),
                None => assert_eq!(components.get(i).map(|(x, _)| x), None),
            }
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i).map(|(_, x)| x), Some(&x as &Rot)),
                None => assert_eq!(components.get(i).map(|(_, x)| x), None),
            }
        }
    }

    #[test]
    fn delete_first() {
        let mut world = create();

        let shared = (Static, Model(5));
        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert(shared, components.clone()).to_vec();

        let first = *entities.first().unwrap();
        world.delete(first);

        for (i, e) in entities.iter().skip(1).enumerate() {
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i + 1).map(|(x, _)| x), Some(&x as &Pos)),
                None => assert_eq!(components.get(i + 1).map(|(x, _)| x), None),
            }
            match world.get_component(*e) {
                Some(x) => assert_eq!(components.get(i + 1).map(|(_, x)| x), Some(&x as &Rot)),
                None => assert_eq!(components.get(i + 1).map(|(_, x)| x), None),
            }
        }
    }

    #[test]
    fn add_component() {
        let mut world = create();

        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert((Static,), components.clone()).to_vec();

        for (i, e) in entities.iter().enumerate() {
            world.add_component(*e, Scale(2., 2., 2.));
            assert_eq!(
                components.get(i).unwrap().0,
                *world.get_component(*e).unwrap()
            );
            assert_eq!(
                components.get(i).unwrap().1,
                *world.get_component(*e).unwrap()
            );
            assert_eq!(Scale(2., 2., 2.), *world.get_component(*e).unwrap());
        }
    }

    #[test]
    fn remove_component() {
        let mut world = create();

        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert((Static,), components.clone()).to_vec();

        for (i, e) in entities.iter().enumerate() {
            world.remove_component::<Rot>(*e);
            assert_eq!(
                components.get(i).unwrap().0,
                *world.get_component(*e).unwrap()
            );
            assert!(world.get_component::<Rot>(*e).is_none());
        }
    }

    #[test]
    fn add_tag() {
        let mut world = create();

        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert((Static,), components.clone()).to_vec();

        for (i, e) in entities.iter().enumerate() {
            world.add_tag(*e, Model(2));
            assert_eq!(
                components.get(i).unwrap().0,
                *world.get_component(*e).unwrap()
            );
            assert_eq!(
                components.get(i).unwrap().1,
                *world.get_component(*e).unwrap()
            );
            assert_eq!(Static, *world.get_tag(*e).unwrap());
            assert_eq!(Model(2), *world.get_tag(*e).unwrap());
        }
    }

    #[test]
    fn remove_tag() {
        let mut world = create();

        let components = vec![
            (Pos(1., 2., 3.), Rot(0.1, 0.2, 0.3)),
            (Pos(4., 5., 6.), Rot(0.4, 0.5, 0.6)),
        ];

        let entities = world.insert((Static,), components.clone()).to_vec();

        for (i, e) in entities.iter().enumerate() {
            world.remove_tag::<Static>(*e);
            assert_eq!(
                components.get(i).unwrap().0,
                *world.get_component(*e).unwrap()
            );
            assert_eq!(
                components.get(i).unwrap().1,
                *world.get_component(*e).unwrap()
            );
            assert!(world.get_tag::<Static>(*e).is_none());
        }
    }
}
