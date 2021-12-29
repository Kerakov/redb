use crate::tree_store::page_store::page_allocator::PageAllocator;
use crate::tree_store::page_store::utils::get_page_size;
use crate::Error;
use memmap2::MmapMut;
use std::cell::RefCell;
use std::collections::HashSet;
use std::convert::TryInto;
use std::fmt::{Debug, Formatter};
use std::mem::size_of;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

const DB_METADATA_PAGE: u64 = 0;

const MAGICNUMBER: [u8; 4] = [b'r', b'e', b'd', b'b'];
const VERSION_OFFSET: usize = MAGICNUMBER.len();
const PAGE_SIZE_OFFSET: usize = VERSION_OFFSET + 1;
const DB_SIZE_OFFSET: usize = PAGE_SIZE_OFFSET + size_of::<u64>();
const PRIMARY_BIT_OFFSET: usize = DB_SIZE_OFFSET + size_of::<u64>();
const TRANSACTION_SIZE: usize = 128;
const TRANSACTION_0_OFFSET: usize = 128;
const TRANSACTION_1_OFFSET: usize = TRANSACTION_0_OFFSET + TRANSACTION_SIZE;
const DB_METAPAGE_SIZE: usize = TRANSACTION_1_OFFSET + TRANSACTION_SIZE;

// Structure of each metapage
const ROOT_PAGE_OFFSET: usize = 0;
const ROOT_PAGE_MESSAGE_BYTES_OFFSET: usize = ROOT_PAGE_OFFSET + size_of::<u64>();
const TRANSACTION_ID_OFFSET: usize = ROOT_PAGE_MESSAGE_BYTES_OFFSET + size_of::<u32>();
// Memory pointed to by this ptr is logically part of the metapage
const ALLOCATOR_STATE_PTR_OFFSET: usize = TRANSACTION_ID_OFFSET + size_of::<u128>();
const ALLOCATOR_STATE_LEN_OFFSET: usize = ALLOCATOR_STATE_PTR_OFFSET + size_of::<u64>();
// TODO: these dirty flags should be part of the PRIMARY_BIT byte, so that they can be written atomically
const ALLOCATOR_STATE_DIRTY_OFFSET: usize = ALLOCATOR_STATE_LEN_OFFSET + size_of::<u64>();

// Marker struct for the mutex guarding the meta page
struct MetapageGuard;

fn get_primary(metapage: &[u8]) -> &[u8] {
    let start = if metapage[PRIMARY_BIT_OFFSET] == 0 {
        TRANSACTION_0_OFFSET
    } else {
        TRANSACTION_1_OFFSET
    };
    let end = start + TRANSACTION_SIZE;

    &metapage[start..end]
}

// Warning! This method is only safe to use when modifying the allocator state and when the dirty bit
// is already set and fsync'ed to the backing file
fn get_primary_mut(metapage: &mut [u8]) -> &mut [u8] {
    let start = if metapage[PRIMARY_BIT_OFFSET] == 0 {
        TRANSACTION_0_OFFSET
    } else {
        TRANSACTION_1_OFFSET
    };
    let end = start + TRANSACTION_SIZE;

    &mut metapage[start..end]
}

fn get_secondary(metapage: &mut [u8]) -> &mut [u8] {
    let start = if metapage[PRIMARY_BIT_OFFSET] == 0 {
        TRANSACTION_1_OFFSET
    } else {
        TRANSACTION_0_OFFSET
    };
    let end = start + TRANSACTION_SIZE;

    &mut metapage[start..end]
}

fn get_secondary_const(metapage: &[u8]) -> &[u8] {
    let start = if metapage[PRIMARY_BIT_OFFSET] == 0 {
        TRANSACTION_1_OFFSET
    } else {
        TRANSACTION_0_OFFSET
    };
    let end = start + TRANSACTION_SIZE;

    &metapage[start..end]
}

#[derive(Copy, Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub(crate) struct PageNumber {
    page_index: u64,
    page_order: u8,
}

impl PageNumber {
    // TODO: remove this
    pub(crate) fn null() -> Self {
        Self::new(0, 0)
    }

    fn new(page_index: u64, page_order: u8) -> Self {
        Self {
            page_index,
            page_order,
        }
    }

    pub(crate) fn to_be_bytes(self) -> [u8; 8] {
        let mut temp = self.page_index;
        temp |= (self.page_order as u64) << 48;
        temp.to_be_bytes()
    }

    pub(crate) fn from_be_bytes(bytes: [u8; 8]) -> Self {
        let temp = u64::from_be_bytes(bytes);
        let index = temp & 0x0000_FFFF_FFFF_FFFF;
        let order = (temp >> 48) as u8;

        Self::new(index, order)
    }

    fn address_range(&self, page_size: usize) -> Range<usize> {
        let pages = 1usize << self.page_order;
        (self.page_index as usize * pages * page_size)
            ..((self.page_index as usize + 1) * pages * page_size)
    }

    fn page_size_bytes(&self, page_size: usize) -> usize {
        let pages = 1usize << self.page_order;
        pages * page_size
    }
}

struct TransactionAccessor<'a> {
    mem: &'a [u8],
    _guard: MutexGuard<'a, MetapageGuard>,
}

impl<'a> TransactionAccessor<'a> {
    fn new(mem: &'a [u8], guard: MutexGuard<'a, MetapageGuard>) -> Self {
        TransactionAccessor { mem, _guard: guard }
    }

    fn get_root_page(&self) -> Option<(PageNumber, u32)> {
        let num = PageNumber::from_be_bytes(
            self.mem[ROOT_PAGE_OFFSET..(ROOT_PAGE_OFFSET + 8)]
                .try_into()
                .unwrap(),
        );
        let message_bytes = u32::from_be_bytes(
            self.mem[ROOT_PAGE_MESSAGE_BYTES_OFFSET
                ..(ROOT_PAGE_MESSAGE_BYTES_OFFSET + size_of::<u32>())]
                .try_into()
                .unwrap(),
        );
        if num.page_index == 0 {
            None
        } else {
            Some((num, message_bytes))
        }
    }

    fn get_last_committed_transaction_id(&self) -> u128 {
        u128::from_be_bytes(
            self.mem[TRANSACTION_ID_OFFSET..(TRANSACTION_ID_OFFSET + size_of::<u128>())]
                .try_into()
                .unwrap(),
        )
    }

    #[allow(dead_code)]
    fn get_allocator_data(&self) -> (usize, usize) {
        let start = u64::from_be_bytes(
            self.mem[ALLOCATOR_STATE_PTR_OFFSET..(ALLOCATOR_STATE_PTR_OFFSET + size_of::<u64>())]
                .try_into()
                .unwrap(),
        );
        let len = u64::from_be_bytes(
            self.mem[ALLOCATOR_STATE_LEN_OFFSET..(ALLOCATOR_STATE_LEN_OFFSET + size_of::<u64>())]
                .try_into()
                .unwrap(),
        );
        (start as usize, (start + len) as usize)
    }

    fn get_allocator_dirty(&self) -> bool {
        let value = u8::from_be_bytes(
            self.mem
                [ALLOCATOR_STATE_DIRTY_OFFSET..(ALLOCATOR_STATE_DIRTY_OFFSET + size_of::<u8>())]
                .try_into()
                .unwrap(),
        );
        match value {
            0 => false,
            1 => true,
            _ => unreachable!(),
        }
    }
}

struct TransactionMutator<'a> {
    mem: &'a mut [u8],
    _guard: MutexGuard<'a, MetapageGuard>,
}

impl<'a> TransactionMutator<'a> {
    fn new(mem: &'a mut [u8], guard: MutexGuard<'a, MetapageGuard>) -> Self {
        TransactionMutator { mem, _guard: guard }
    }

    fn set_root_page(&mut self, page_number: PageNumber, valid_message_bytes: u32) {
        self.mem[ROOT_PAGE_OFFSET..(ROOT_PAGE_OFFSET + 8)]
            .copy_from_slice(&page_number.to_be_bytes());
        self.mem[ROOT_PAGE_MESSAGE_BYTES_OFFSET..(ROOT_PAGE_MESSAGE_BYTES_OFFSET + 4)]
            .copy_from_slice(&valid_message_bytes.to_be_bytes());
    }

    fn set_last_committed_transaction_id(&mut self, transaction_id: u128) {
        self.mem[TRANSACTION_ID_OFFSET..(TRANSACTION_ID_OFFSET + size_of::<u128>())]
            .copy_from_slice(&transaction_id.to_be_bytes());
    }

    fn set_allocator_data(&mut self, start: usize, len: usize) {
        self.mem[ALLOCATOR_STATE_PTR_OFFSET..(ALLOCATOR_STATE_PTR_OFFSET + size_of::<u64>())]
            .copy_from_slice(&(start as u64).to_be_bytes());
        self.mem[ALLOCATOR_STATE_LEN_OFFSET..(ALLOCATOR_STATE_LEN_OFFSET + size_of::<u64>())]
            .copy_from_slice(&(len as u64).to_be_bytes());
    }

    fn get_allocator_data(&self) -> (usize, usize) {
        let start = u64::from_be_bytes(
            self.mem[ALLOCATOR_STATE_PTR_OFFSET..(ALLOCATOR_STATE_PTR_OFFSET + size_of::<u64>())]
                .try_into()
                .unwrap(),
        );
        let len = u64::from_be_bytes(
            self.mem[ALLOCATOR_STATE_LEN_OFFSET..(ALLOCATOR_STATE_LEN_OFFSET + size_of::<u64>())]
                .try_into()
                .unwrap(),
        );
        (start as usize, (start + len) as usize)
    }

    fn set_allocator_dirty(&mut self, dirty: bool) {
        if dirty {
            self.mem[ALLOCATOR_STATE_DIRTY_OFFSET] = 1;
        } else {
            self.mem[ALLOCATOR_STATE_DIRTY_OFFSET] = 0;
        }
    }

    fn get_allocator_dirty(&self) -> bool {
        let value = u8::from_be_bytes(
            self.mem
                [ALLOCATOR_STATE_DIRTY_OFFSET..(ALLOCATOR_STATE_DIRTY_OFFSET + size_of::<u8>())]
                .try_into()
                .unwrap(),
        );
        match value {
            0 => false,
            1 => true,
            _ => unreachable!(),
        }
    }

    fn into_guard(self) -> MutexGuard<'a, MetapageGuard> {
        self._guard
    }
}

pub(crate) trait Page {
    fn memory(&self) -> &[u8];

    fn get_page_number(&self) -> PageNumber;
}

pub struct PageImpl<'a> {
    mem: &'a [u8],
    page_number: PageNumber,
}

impl<'a> Debug for PageImpl<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("PageImpl: page_number={:?}", self.page_number))
    }
}

impl<'a> Page for PageImpl<'a> {
    fn memory(&self) -> &[u8] {
        self.mem
    }

    fn get_page_number(&self) -> PageNumber {
        self.page_number
    }
}

pub(crate) struct PageMut<'a> {
    mem: &'a mut [u8],
    page_number: PageNumber,
    open_pages: &'a RefCell<HashSet<PageNumber>>,
}

impl<'a> PageMut<'a> {
    pub(crate) fn memory_mut(&mut self) -> &mut [u8] {
        self.mem
    }
}

impl<'a> Page for PageMut<'a> {
    fn memory(&self) -> &[u8] {
        self.mem
    }

    fn get_page_number(&self) -> PageNumber {
        self.page_number
    }
}

impl<'a> Drop for PageMut<'a> {
    fn drop(&mut self) {
        self.open_pages.borrow_mut().remove(&self.page_number);
    }
}

pub(crate) struct TransactionalMemory {
    // Pages allocated since the last commit
    allocated_since_commit: RefCell<HashSet<PageNumber>>,
    freed_since_commit: RefCell<Vec<PageNumber>>,
    // Metapage guard lock should be held when using this to modify the page allocator state
    page_allocator: PageAllocator,
    mmap: MmapMut,
    // We use unsafe to access the metapage (page 0), and so guard it with this mutex
    // It would be nice if this was a RefCell<&[u8]> on the metapage. However, that would be
    // self-referential, since we also hold the mmap object
    metapage_guard: Mutex<MetapageGuard>,
    // The number of PageMut which are outstanding
    open_dirty_pages: RefCell<HashSet<PageNumber>>,
    // Indicates that a non-durable commit has been made, so reads should be served from the secondary meta page
    read_from_secondary: AtomicBool,
    page_size: usize,
}

impl TransactionalMemory {
    fn calculate_usable_pages(mmap_size: usize) -> usize {
        let mut guess = mmap_size / get_page_size();
        let mut new_guess =
            (mmap_size - 2 * PageAllocator::required_space(guess)) / get_page_size();
        // Make sure we don't loop forever. This might not converge if it oscillates
        let mut i = 0;
        while guess != new_guess && i < 1000 {
            guess = new_guess;
            new_guess = (mmap_size - 2 * PageAllocator::required_space(guess)) / get_page_size();
            i += 1;
        }

        guess
    }

    pub(crate) fn new(
        mut mmap: MmapMut,
        requested_page_size: Option<usize>,
    ) -> Result<Self, Error> {
        let mutex = Mutex::new(MetapageGuard {});
        let usable_pages = Self::calculate_usable_pages(mmap.len());
        let page_allocator = PageAllocator::new(usable_pages);
        if mmap[0..MAGICNUMBER.len()] != MAGICNUMBER {
            let page_size = requested_page_size.unwrap_or_else(get_page_size);
            // Ensure that the database metadata fits into the first page
            assert!(page_size >= DB_METAPAGE_SIZE);
            assert!(page_size.is_power_of_two());

            // Explicitly zero the memory
            mmap[0..DB_METAPAGE_SIZE].copy_from_slice(&[0; DB_METAPAGE_SIZE]);
            for i in &mut mmap[(usable_pages * page_size)..] {
                *i = 0
            }

            let allocator_state_size = PageAllocator::required_space(usable_pages);

            // Store the page & db size. These are immutable
            mmap[PAGE_SIZE_OFFSET] = page_size.trailing_zeros() as u8;
            let length = mmap.len() as u64;
            mmap[DB_SIZE_OFFSET..(DB_SIZE_OFFSET + size_of::<u64>())]
                .copy_from_slice(&length.to_be_bytes());

            // Set to 1, so that we can mutate the first transaction state
            mmap[PRIMARY_BIT_OFFSET] = 1;
            let start = mmap.len() - 2 * allocator_state_size;
            let mut mutator =
                TransactionMutator::new(get_secondary(&mut mmap), mutex.lock().unwrap());
            mutator.set_root_page(PageNumber::new(0, 0), 0);
            mutator.set_last_committed_transaction_id(0);
            mutator.set_allocator_dirty(false);
            mutator.set_allocator_data(start, allocator_state_size);
            drop(mutator);
            let allocator = PageAllocator::init_new(
                &mut mmap[start..(start + allocator_state_size)],
                usable_pages,
            );
            allocator.record_alloc(
                &mut mmap[start..(start + allocator_state_size)],
                DB_METADATA_PAGE,
            );
            // Make the state we just wrote the primary
            mmap[PRIMARY_BIT_OFFSET] = 0;

            // Initialize the secondary allocator state
            let start = mmap.len() - allocator_state_size;
            let mut mutator =
                TransactionMutator::new(get_secondary(&mut mmap), mutex.lock().unwrap());
            mutator.set_allocator_dirty(false);
            mutator.set_allocator_data(start, allocator_state_size);
            drop(mutator);
            let allocator = PageAllocator::init_new(
                &mut mmap[start..(start + allocator_state_size)],
                usable_pages,
            );
            allocator.record_alloc(
                &mut mmap[start..(start + allocator_state_size)],
                DB_METADATA_PAGE,
            );

            mmap[VERSION_OFFSET] = 1;

            mmap.flush()?;
            // Write the magic number only after the data structure is initialized and written to disk
            // to ensure that it's crash safe
            mmap[0..MAGICNUMBER.len()].copy_from_slice(&MAGICNUMBER);
            mmap.flush()?;
        }

        let page_size = (1 << mmap[PAGE_SIZE_OFFSET]) as usize;
        if let Some(size) = requested_page_size {
            assert_eq!(page_size, size);
        }
        assert_eq!(
            u64::from_be_bytes(
                mmap[DB_SIZE_OFFSET..(DB_SIZE_OFFSET + size_of::<u64>())]
                    .try_into()
                    .unwrap()
            ) as usize,
            mmap.len()
        );

        let accessor = TransactionAccessor::new(get_primary(&mmap), mutex.lock().unwrap());
        // TODO: repair instead of crashing
        assert!(!accessor.get_allocator_dirty());
        drop(accessor);
        let accessor = TransactionAccessor::new(get_secondary(&mut mmap), mutex.lock().unwrap());
        assert!(!accessor.get_allocator_dirty());
        drop(accessor);

        Ok(TransactionalMemory {
            allocated_since_commit: RefCell::new(HashSet::new()),
            freed_since_commit: RefCell::new(vec![]),
            page_allocator,
            mmap,
            metapage_guard: mutex,
            open_dirty_pages: RefCell::new(HashSet::new()),
            read_from_secondary: AtomicBool::new(false),
            page_size,
        })
    }

    fn acquire_mutable_metapage(&self) -> (&mut [u8], MutexGuard<MetapageGuard>) {
        let guard = self.metapage_guard.lock().unwrap();
        let ptr = &self.mmap as *const MmapMut as *mut MmapMut;
        // Safety: we acquire the metapage lock and only access the metapage
        let mem = unsafe { &mut (*ptr)[0..DB_METAPAGE_SIZE] };

        (mem, guard)
    }

    fn acquire_mutable_page_allocator<'a>(
        &self,
        mut mutator: TransactionMutator<'a>,
    ) -> Result<(&mut [u8], MutexGuard<'a, MetapageGuard>), Error> {
        // The allocator is a cache, and therefore can only be modified when it's marked dirty
        if !mutator.get_allocator_dirty() {
            mutator.set_allocator_dirty(true);
            self.mmap.flush()?
        }

        let ptr = &self.mmap as *const MmapMut as *mut MmapMut;
        // Safety: we have the metapage lock and only access the metapage
        // (page allocator state is logically part of the metapage)
        let (start, end) = mutator.get_allocator_data();
        assert!(end <= self.mmap.len());
        let mem = unsafe { &mut (*ptr)[start..end] };

        Ok((mem, mutator.into_guard()))
    }

    // Commit all outstanding changes and make them visible as the primary
    pub(crate) fn commit(&self, transaction_id: u128) -> Result<(), Error> {
        // All mutable pages must be dropped, this ensures that when a transaction completes
        // no more writes can happen to the pages it allocated. Thus it is safe to make them visible
        // to future read transactions
        assert!(self.open_dirty_pages.borrow().is_empty());

        let (mmap, guard) = self.acquire_mutable_metapage();
        let mut mutator = TransactionMutator::new(get_secondary(mmap), guard);
        mutator.set_last_committed_transaction_id(transaction_id);
        drop(mutator);

        self.mmap.flush()?;

        let next = match self.mmap[PRIMARY_BIT_OFFSET] {
            0 => 1,
            1 => 0,
            _ => unreachable!(),
        };
        let (mmap, guard) = self.acquire_mutable_metapage();
        let mut mutator = TransactionMutator::new(get_secondary(mmap), guard);
        mutator.set_allocator_dirty(false);
        drop(mutator);
        let (mmap, guard) = self.acquire_mutable_metapage();

        mmap[PRIMARY_BIT_OFFSET] = next;
        // Dirty the current primary (we just switched them on the previous line)
        let mut mutator = TransactionMutator::new(get_secondary(mmap), guard);
        mutator.set_allocator_dirty(true);
        self.mmap.flush()?;

        let (mem, guard) = self.acquire_mutable_page_allocator(mutator)?;
        for page_number in self.allocated_since_commit.borrow_mut().drain() {
            assert_eq!(page_number.page_order, 0);
            self.page_allocator
                .record_alloc(mem, page_number.page_index);
        }
        for page_number in self.freed_since_commit.borrow_mut().drain(..) {
            assert_eq!(page_number.page_order, 0);
            self.page_allocator.free(mem, page_number.page_index);
        }
        drop(guard); // Ensure the guard lives past all the writes to the page allocator state
        self.read_from_secondary.store(false, Ordering::SeqCst);

        Ok(())
    }

    // Make changes visible, without a durability guarantee
    pub(crate) fn non_durable_commit(&self, transaction_id: u128) -> Result<(), Error> {
        // All mutable pages must be dropped, this ensures that when a transaction completes
        // no more writes can happen to the pages it allocated. Thus it is safe to make them visible
        // to future read transactions
        assert!(self.open_dirty_pages.borrow().is_empty());

        let (mmap, guard) = self.acquire_mutable_metapage();
        let mut mutator = TransactionMutator::new(get_secondary(mmap), guard);
        mutator.set_last_committed_transaction_id(transaction_id);
        drop(mutator);

        // Ensure the dirty bit is set on the primary page, so that the following updates to it are safe
        let mut primary_mutator =
            TransactionMutator::new(get_primary_mut(mmap), self.metapage_guard.lock().unwrap());
        if !primary_mutator.get_allocator_dirty() {
            primary_mutator.set_allocator_dirty(true);
            // Must fsync this, even though we're in a non-durable commit. Because we're dirtying
            // the primary allocator state
            self.mmap.flush()?;
        }

        // Modify the primary allocator state directly. This is only safe because we first set the dirty bit
        let (mem, guard) = self.acquire_mutable_page_allocator(primary_mutator)?;
        for page_number in self.allocated_since_commit.borrow_mut().drain() {
            assert_eq!(page_number.page_order, 0);
            self.page_allocator
                .record_alloc(mem, page_number.page_index);
        }
        assert!(self.freed_since_commit.borrow().is_empty());
        drop(guard); // Ensure the guard lives past all the writes to the page allocator state
        self.read_from_secondary.store(true, Ordering::SeqCst);

        Ok(())
    }

    pub(crate) fn rollback_uncommited_writes(&self) -> Result<(), Error> {
        assert!(self.open_dirty_pages.borrow().is_empty());
        let (metamem, guard) = self.acquire_mutable_metapage();
        let mutator = TransactionMutator::new(get_secondary(metamem), guard);
        let (mem, guard) = self.acquire_mutable_page_allocator(mutator)?;
        for page_number in self.allocated_since_commit.borrow_mut().drain() {
            assert_eq!(page_number.page_order, 0);
            self.page_allocator.free(mem, page_number.page_index);
        }
        for page_number in self.freed_since_commit.borrow_mut().drain(..) {
            assert_eq!(page_number.page_order, 0);
            self.page_allocator
                .record_alloc(mem, page_number.page_index);
        }
        // Drop guard only after page_allocator calls are completed
        drop(guard);

        Ok(())
    }

    pub(crate) fn get_page(&self, page_number: PageNumber) -> PageImpl {
        // We must not retrieve an immutable reference to a page which already has a mutable ref to it
        assert!(
            !self.open_dirty_pages.borrow().contains(&page_number),
            "{:?}",
            page_number
        );

        PageImpl {
            mem: &self.mmap[page_number.address_range(self.page_size)],
            page_number,
        }
    }

    pub(crate) fn get_page_mut(&self, page_number: PageNumber) -> PageMut {
        self.open_dirty_pages.borrow_mut().insert(page_number);

        let address = &self.mmap as *const MmapMut as *mut MmapMut;
        // Safety:
        // All PageMut are registered in open_dirty_pages, and no immutable references are allowed
        // to those pages
        // TODO: change this to take a NodeHandle, and check that future get_page() calls don't
        // request valid_message bytes after this request. Otherwise, we could get a race.
        // Immutable references are allowed, they just need to be to a strict subset of the
        // valid delta message bytes
        let mem = unsafe { &mut (*address)[page_number.address_range(self.page_size)] };

        PageMut {
            mem,
            page_number,
            open_pages: &self.open_dirty_pages,
        }
    }

    pub(crate) fn get_primary_root_page(&self) -> Option<(PageNumber, u32)> {
        if self.read_from_secondary.load(Ordering::SeqCst) {
            TransactionAccessor::new(
                get_secondary_const(&self.mmap),
                self.metapage_guard.lock().unwrap(),
            )
            .get_root_page()
        } else {
            TransactionAccessor::new(get_primary(&self.mmap), self.metapage_guard.lock().unwrap())
                .get_root_page()
        }
    }

    pub(crate) fn get_last_committed_transaction_id(&self) -> u128 {
        if self.read_from_secondary.load(Ordering::SeqCst) {
            TransactionAccessor::new(
                get_secondary_const(&self.mmap),
                self.metapage_guard.lock().unwrap(),
            )
            .get_last_committed_transaction_id()
        } else {
            TransactionAccessor::new(get_primary(&self.mmap), self.metapage_guard.lock().unwrap())
                .get_last_committed_transaction_id()
        }
    }

    // TODO: valid_message_bytes kind of breaks the separation of concerns for the PageManager.
    // It's only used by the delta message protocol of the b-tree
    pub(crate) fn set_secondary_root_page(&self, root_page: PageNumber, valid_message_bytes: u32) {
        let (mmap, guard) = self.acquire_mutable_metapage();
        let mut mutator = TransactionMutator::new(get_secondary(mmap), guard);
        mutator.set_root_page(root_page, valid_message_bytes);
    }

    pub(crate) fn free(&self, page: PageNumber) {
        let (mmap, guard) = self.acquire_mutable_metapage();
        let mutator = TransactionMutator::new(get_secondary(mmap), guard);
        // TODO: should propagate this error
        let (mem, guard) = self.acquire_mutable_page_allocator(mutator).unwrap();
        assert_eq!(page.page_order, 0);
        self.page_allocator.free(mem, page.page_index);
        drop(guard);
        self.freed_since_commit.borrow_mut().push(page);
    }

    // Frees the page if it was allocated since the last commit. Returns true, if the page was freed
    pub(crate) fn free_if_uncommitted(&self, page: PageNumber) -> bool {
        if self.allocated_since_commit.borrow_mut().remove(&page) {
            let (mmap, guard) = self.acquire_mutable_metapage();
            let mutator = TransactionMutator::new(get_secondary(mmap), guard);
            // TODO: should propagate this error
            let (mem, guard) = self.acquire_mutable_page_allocator(mutator).unwrap();
            assert_eq!(page.page_order, 0);
            self.page_allocator.free(mem, page.page_index);
            drop(guard);
            true
        } else {
            false
        }
    }

    // Page has not been committed
    pub(crate) fn uncommitted(&self, page: PageNumber) -> bool {
        self.allocated_since_commit.borrow().contains(&page)
    }

    pub(crate) fn allocate(&self, allocation_size: usize) -> PageMut {
        assert!(allocation_size <= self.page_size);

        let (mmap, guard) = self.acquire_mutable_metapage();
        let mutator = TransactionMutator::new(get_secondary(mmap), guard);
        // TODO: should propagate this error
        let (mem, guard) = self.acquire_mutable_page_allocator(mutator).unwrap();
        // TODO: handle out-of-memory and return an error
        let page_number = PageNumber::new(self.page_allocator.alloc(mem).unwrap(), 0);
        // Drop guard only after page_allocator.alloc() is completed
        drop(guard);

        self.allocated_since_commit.borrow_mut().insert(page_number);
        self.open_dirty_pages.borrow_mut().insert(page_number);

        let address = &self.mmap as *const MmapMut as *mut MmapMut;

        let address_range = page_number.address_range(self.page_size);
        assert!(address_range.end <= self.mmap.len());
        // Safety:
        // All PageMut are registered in open_dirty_pages, and no immutable references are allowed
        // to those pages
        let mem = unsafe { &mut (*address)[address_range] };
        // Zero the memory
        mem.copy_from_slice(&vec![0u8; page_number.page_size_bytes(self.page_size)]);

        PageMut {
            mem,
            page_number,
            open_pages: &self.open_dirty_pages,
        }
    }

    pub(crate) fn count_free_pages(&self) -> usize {
        let (mmap, guard) = self.acquire_mutable_metapage();
        // TODO: this is a read-only operation, so should be able to use an accessor
        // and avoid dirtying the allocator state
        let mutator = TransactionMutator::new(get_secondary(mmap), guard);
        let (mem, guard) = self.acquire_mutable_page_allocator(mutator).unwrap();
        let count = self.page_allocator.count_free_pages(mem);
        // Drop guard only after page_allocator.count_free() is completed
        drop(guard);

        count
    }
}

impl Drop for TransactionalMemory {
    fn drop(&mut self) {
        // Commit any non-durable transactions that are outstanding
        if self.read_from_secondary.load(Ordering::SeqCst) {
            let non_durable_transaction_id = self.get_last_committed_transaction_id();
            self.commit(non_durable_transaction_id)
                .expect("Failure while finalizing non-durable commit. Database may be corrupted");
        }
        if self.mmap.flush().is_ok() {
            let (metamem, guard) = self.acquire_mutable_metapage();
            let mut mutator = TransactionMutator::new(get_secondary(metamem), guard);
            mutator.set_allocator_dirty(false);
            let _ = self.mmap.flush();
        }
    }
}
