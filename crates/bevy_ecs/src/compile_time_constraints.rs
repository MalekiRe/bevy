pub mod constime {
    use core::fmt::Debug;
    use std::prelude::rust_2024::{Box, Vec};

    #[derive(Debug)]
    pub enum TreeInner<T: 'static + Debug> {
        Empty,
        Leaf(T),
        Node(Tree<T>, Tree<T>)
    }
    pub type Tree<T> = &'static TreeInner<T>;

    impl<T: 'static + Debug> TreeInner<T> {
        const EMPTY: Tree<T> = &TreeInner::Empty;
    }



    #[derive(Debug)]
    pub enum WorldQueryInner {
        Empty,
        Leaf(&'static WorldQueryInner),
        Node(&'static WorldQueryInner, &'static WorldQueryInner),
        Ref(Id),
        Mut(Id),
        With(Id),
        Without(Id),
        Added(Id),
        Changed(Id),
        Has(Id),
        Option(WorldQuery),
        Or(WorldQuery),
        And(WorldQuery),
        AnyOf(WorldQuery),
        FilteredEntityMut(WorldQuery),
        FilteredEntityRef(WorldQuery),
        EntityMutExcept(),
        EntityMut,
        EntityRef,
        Archetype,
    }

    impl WorldQueryInner {
        pub const EMPTY: WorldQuery = &WorldQueryInner::Empty;
    }

    pub type WorldQuery = &'static WorldQueryInner;

    #[derive(Debug)]
    pub struct FilteredAccessSet {
        combined_access: Access,
        filtered_accesses: Tree<FilteredAccess>,
    }

    #[derive(Debug)]
    pub struct FilteredAccess {
        pub access: Access,
        pub required: Tree<Id>,
        pub filter_sets: Tree<AccessFilters>,
    }

    #[derive(Debug)]
    pub struct Access {
        /// All accessed components, or forbidden components if
        /// `Self::component_read_and_writes_inverted` is set.
        component_reads_and_writes: [Option<Id>; 256],
        /// All exclusively-accessed components, or components that may not be
        /// exclusively accessed if `Self::component_writes_inverted` is set.
        component_writes: [Option<Id>; 256],
        /// All accessed resources.
        resource_read_and_writes: [Option<Id>; 256],
        /// The exclusively-accessed resources.
        resource_writes: [Option<Id>; 256],
        /// Is `true` if this component can read all components *except* those
        /// present in `Self::component_read_and_writes`.
        component_read_and_writes_inverted: bool,
        /// Is `true` if this component can write to all components *except* those
        /// present in `Self::component_writes`.
        component_writes_inverted: bool,
        /// Is `true` if this has access to all resources.
        /// This field is a performance optimization for `&World` (also harder to mess up for soundness).
        reads_all_resources: bool,
        /// Is `true` if this has mutable access to all resources.
        /// If this is true, then `reads_all` must also be true.
        writes_all_resources: bool,
    }

    #[derive(Debug)]
    pub struct AccessFilters {
        with: Tree<Id>,
        without: Tree<Id>,
    }

    pub type ComponentTree = Tree<Id>;
    pub type ComponentTreeInner = TreeInner<Id>;
    pub type ResourceTree = Tree<Id>;
    pub type ResourceTreeInner = TreeInner<Id>;

    #[derive(Copy, Clone, Debug)]
    pub struct Id(u128);

    impl Id {
        pub const fn new(val: u128) -> Self {
            Self(val)
        }
        pub const fn eq(&self, rhs: &Self) -> bool {
            self.0 == rhs.0
        }
    }

    impl FilteredAccess {
        pub const fn new(world_query: WorldQuery) -> Self {
            let mut world_query = world_query;

            todo!()
        }
    }

    trait A<const N: usize> {
        const LIST: [u32; N + 1];
    }
    pub struct B;
    impl<const N: usize> A<N> for B {
        const LIST: [u32; N + 1] = B::LIST;
    }
}
