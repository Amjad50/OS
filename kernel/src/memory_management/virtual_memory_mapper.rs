//! This very specific to 64-bit x86 architecture, if this is to be ported to other architectures
//! this will need to be changed

use core::{ops::RangeBounds, slice::IterMut};

use crate::{
    cpu,
    memory_management::{
        memory_layout::{
            align_range, align_up, is_aligned, kernel_elf_rodata_end, physical2virtual,
            virtual2physical, MemSize, EXTENDED_OFFSET, KERNEL_BASE, KERNEL_LINK,
            KERNEL_MAPPED_SIZE, PAGE_2M, PAGE_4K,
        },
        physical_page_allocator,
    },
    sync::spin::mutex::Mutex,
};

use super::memory_layout::{
    stack_guard_page_ptr, PROCESS_KERNEL_STACK_BASE, PROCESS_KERNEL_STACK_SIZE,
};

// TODO: replace by some sort of bitfield
#[allow(dead_code)]
pub mod flags {
    pub(super) const PTE_PRESENT: u64 = 1 << 0;
    pub const PTE_WRITABLE: u64 = 1 << 1;
    pub const PTE_USER: u64 = 1 << 2;
    pub const PTE_WRITETHROUGH: u64 = 1 << 3;
    pub const PTE_NOT_CACHEABLE: u64 = 1 << 4;
    pub(super) const PTE_ACCESSED: u64 = 1 << 5;
    pub(super) const PTE_DIRTY: u64 = 1 << 6;
    pub(super) const PTE_HUGE_PAGE: u64 = 1 << 7;
    pub(super) const PTE_GLOBAL: u64 = 1 << 8;
    pub(super) const PTE_NO_EXECUTE: u64 = 1 << 63;
}

const ADDR_MASK: u64 = 0x0000_0000_FFFF_F000;

// only use the last index for the kernel
// all the other indexes are free to use by the user
const KERNEL_L4_INDEX: usize = 0x1FF;

// The L3 positions are used for the non-moving kernel code/data
const KERNEL_L3_INDEX_START: usize = 0x1FE;
#[allow(dead_code)]
const KERNEL_L3_INDEX_END: usize = 0x1FF;

const KERNEL_L3_PROCESS_INDEX_START: usize = 0;
const KERNEL_L3_PROCESS_INDEX_END: usize = KERNEL_L3_INDEX_START - 1;

pub const KERNEL_PROCESS_VIRTUAL_ADDRESS_START: usize =
    // sign extension
    0xFFFF_0000_0000_0000 | KERNEL_L4_INDEX << 39 | KERNEL_L3_PROCESS_INDEX_START << 30;

// the user can use all the indexes except the last one
const NUM_USER_L4_INDEXES: usize = KERNEL_L4_INDEX;

pub const MAX_USER_VIRTUAL_ADDRESS: usize =
    // sign extension
    0xFFFF_0000_0000_0000
        | (KERNEL_L4_INDEX - 1) << 39
        | (0x1FF << 30)
        | (0x1FF << 21)
        | (0x1FF << 12);

#[inline(always)]
const fn get_l4(addr: u64) -> u64 {
    (addr >> 39) & 0x1FF
}

#[inline(always)]
const fn get_l3(addr: u64) -> u64 {
    (addr >> 30) & 0x1FF
}

#[inline(always)]
const fn get_l2(addr: u64) -> u64 {
    (addr >> 21) & 0x1FF
}

#[inline(always)]
const fn get_l1(addr: u64) -> u64 {
    (addr >> 12) & 0x1FF
}

// have a specific alignment so we can fit them in a page
#[repr(C, align(32))]
#[derive(Debug, Copy, Clone)]
pub struct VirtualMemoryMapEntry {
    pub virtual_address: u64,
    pub physical_address: Option<u64>,
    pub size: u64,
    pub flags: u64,
}

// This is a general structure for all levels
#[repr(C, align(4096))]
struct PageDirectoryTable {
    entries: [u64; 512],
}

#[repr(transparent)]
struct PageDirectoryTablePtr(pub u64);

impl PageDirectoryTablePtr {
    fn from_entry(entry: u64) -> Self {
        Self(physical2virtual((entry & ADDR_MASK) as _) as _)
    }

    /// An ugly hack used in `do_for_every_user_entry` to get a mutable reference to the page directory table
    fn enteries_from_mut_entry(entry: &mut u64) -> &mut PageDirectoryTable {
        let table = physical2virtual((*entry & ADDR_MASK) as _) as *mut PageDirectoryTable;
        unsafe { &mut *table }
    }

    fn to_physical(&self) -> u64 {
        virtual2physical(self.0 as _) as _
    }

    fn alloc_new() -> Self {
        // SAFETY: it will panic if it couldn't allocate, so if it returns, it is safe
        Self(unsafe { physical_page_allocator::alloc_zeroed() } as _)
    }

    fn as_ptr(&self) -> *mut PageDirectoryTable {
        self.0 as *mut PageDirectoryTable
    }

    fn as_mut(&mut self) -> &mut PageDirectoryTable {
        unsafe { &mut *self.as_ptr() }
    }

    fn as_ref(&self) -> &PageDirectoryTable {
        unsafe { &*self.as_ptr() }
    }

    unsafe fn free(self) {
        unsafe { physical_page_allocator::free(self.0 as _) };
    }
}

static KERNEL_VIRTUAL_MEMORY_MANAGER: Mutex<VirtualMemoryMapper> =
    Mutex::new(VirtualMemoryMapper::boot_vm());

pub fn init_kernel_vm() {
    let new_kernel_manager = VirtualMemoryMapper::new_kernel_vm();
    let mut manager = KERNEL_VIRTUAL_MEMORY_MANAGER.lock();
    *manager = new_kernel_manager;
    // SAFETY: this is the start VM, so we are sure that we are not inside a process, so its safe to switch
    unsafe { manager.switch_to_this() };
}
/// # Safety
/// This must never be called while we are in a process context
/// and using any process specific memory regions
pub unsafe fn switch_to_kernel() {
    KERNEL_VIRTUAL_MEMORY_MANAGER.lock().switch_to_this();
}

pub fn map_kernel(entry: &VirtualMemoryMapEntry) {
    // make sure we are only mapping to kernel memory
    assert!(entry.virtual_address >= KERNEL_BASE as u64);
    KERNEL_VIRTUAL_MEMORY_MANAGER.lock().map(entry);
}

/// `is_allocated` is used to indicate if the physical pages were allocated by the caller
/// i.e. when we called `map_kernel`, the `physical_address` is `None` and we will allocate the pages, and thus
/// when calling this function, you should pass `is_allocated = true`
// TODO: maybe its better to keep track of this information somewhere in the mapper here
pub fn unmap_kernel(entry: &VirtualMemoryMapEntry, is_allocated: bool) {
    // make sure we are only mapping to kernel memory
    assert!(entry.virtual_address >= KERNEL_BASE as u64);
    KERNEL_VIRTUAL_MEMORY_MANAGER
        .lock()
        .unmap(entry, is_allocated);
}

#[allow(dead_code)]
pub fn is_address_mapped_in_kernel(addr: u64) -> bool {
    KERNEL_VIRTUAL_MEMORY_MANAGER.lock().is_address_mapped(addr)
}

pub fn clone_current_vm_as_user() -> VirtualMemoryMapper {
    // precaution, a sort of manual lock
    cpu::cpu().push_cli();
    let manager = get_current_vm();
    let mut new_vm = manager.clone_kernel_mem();
    cpu::cpu().pop_cli();
    new_vm.is_user = true;
    new_vm
}

pub fn get_current_vm() -> VirtualMemoryMapper {
    VirtualMemoryMapper::get_current_vm()
}

pub struct VirtualMemoryMapper {
    page_map_l4: PageDirectoryTablePtr,
    is_user: bool,
}

impl VirtualMemoryMapper {
    /// Return the VM for the CPU at boot time (only applied to the first CPU and this is setup in `boot.S`)
    const fn boot_vm() -> Self {
        Self {
            // use the same address we used in the assembly code
            // we will change this anyway in `new_kernel_vm`, but at least lets have a valid address
            page_map_l4: PageDirectoryTablePtr(physical2virtual(0x1000) as _),
            is_user: false,
        }
    }

    fn new() -> Self {
        Self {
            page_map_l4: PageDirectoryTablePtr::alloc_new(),
            is_user: false,
        }
    }

    // create a new virtual memory that maps the kernel only
    pub fn clone_kernel_mem(&self) -> Self {
        let this_kernel_l4 =
            PageDirectoryTablePtr::from_entry(self.page_map_l4.as_ref().entries[KERNEL_L4_INDEX]);

        let mut new_vm = Self::new();

        let mut new_kernel_l4 = PageDirectoryTablePtr::alloc_new();

        // copy the whole kernel mapping (process specific will be replaced later)
        for i in 0..=0x1FF {
            new_kernel_l4.as_mut().entries[i] = this_kernel_l4.as_ref().entries[i];
        }

        new_vm.page_map_l4.as_mut().entries[KERNEL_L4_INDEX] =
            new_kernel_l4.to_physical() | flags::PTE_PRESENT | flags::PTE_WRITABLE;

        new_vm
    }

    /// # Safety
    ///
    /// After this call, the VM must never be switched to unless
    /// its from the scheduler or we are sure that the previous kernel regions are not used
    pub unsafe fn add_process_specific_mappings(&mut self) {
        let mut this_kernel_l4 =
            PageDirectoryTablePtr::from_entry(self.page_map_l4.as_ref().entries[KERNEL_L4_INDEX]);

        // clear out the process specific mappings if we have cloned another process
        // but of course don't deallocate, just remove the mappings
        for i in KERNEL_L3_PROCESS_INDEX_START..=KERNEL_L3_PROCESS_INDEX_END {
            this_kernel_l4.as_mut().entries[i] = 0;
        }
        // set it temporarily so we can map kernel range
        // TODO: fix this hack
        self.is_user = false;
        // load new kernel stack for this process
        self.map(&VirtualMemoryMapEntry {
            virtual_address: PROCESS_KERNEL_STACK_BASE as u64,
            physical_address: None, // allocate
            size: PROCESS_KERNEL_STACK_SIZE as u64,
            flags: flags::PTE_WRITABLE,
        });
        self.is_user = true;
    }

    fn load_vm(base: &PageDirectoryTablePtr) {
        eprintln!(
            "Switching to new page map: {:p}",
            virtual2physical(base.0 as _) as *const u8
        );
        unsafe { cpu::set_cr3(base.to_physical()) }
    }

    fn get_current_vm() -> Self {
        let kernel_vm_addr = KERNEL_VIRTUAL_MEMORY_MANAGER.lock().page_map_l4.0;
        let cr3 = physical2virtual(unsafe { cpu::get_cr3() } as _) as _;
        let is_user = cr3 != kernel_vm_addr;
        Self {
            page_map_l4: PageDirectoryTablePtr(cr3),
            is_user,
        }
    }

    /// # Safety
    /// This must be used with caution, it must never be switched while we are using
    /// memory from the same regions, i.e. kernel stack while we are in an interrupt
    pub unsafe fn switch_to_this(&self) {
        Self::load_vm(&self.page_map_l4);
    }

    // This replicate what is done in the assembly code
    // but it will be stored
    fn new_kernel_vm() -> Self {
        let data_start = align_up(kernel_elf_rodata_end(), PAGE_4K);
        let kernel_vm = [
            // Low memory (has some BIOS stuff): mapped to kernel space
            VirtualMemoryMapEntry {
                virtual_address: KERNEL_BASE as u64,
                physical_address: Some(0),
                size: EXTENDED_OFFSET as u64,
                flags: flags::PTE_WRITABLE,
            },
            // Extended memory: kernel .text and .rodata sections
            VirtualMemoryMapEntry {
                virtual_address: KERNEL_LINK as u64,
                physical_address: Some(virtual2physical(KERNEL_LINK) as u64),
                size: virtual2physical(data_start) as u64 - virtual2physical(KERNEL_LINK) as u64,
                flags: 0, // read-only
            },
            // Extended memory: kernel .data and .bss sections and the rest of the data for the `whole` memory
            // we decided to use in the kernel
            VirtualMemoryMapEntry {
                virtual_address: data_start as u64,
                physical_address: Some(virtual2physical(data_start) as u64),
                size: KERNEL_MAPPED_SIZE as u64 - virtual2physical(data_start) as u64,
                flags: flags::PTE_WRITABLE,
            },
        ];

        // create a new fresh page map
        // SAFETY: we are calling the virtual memory manager after initializing the physical page allocator
        let mut s = Self::new();

        for entry in kernel_vm.iter() {
            s.map(entry);
        }

        // unmap stack guard
        s.unmap(
            &VirtualMemoryMapEntry {
                virtual_address: stack_guard_page_ptr() as u64,
                physical_address: None,
                size: PAGE_4K as u64,
                flags: 0,
            },
            false,
        );

        s
    }

    pub fn map(&mut self, entry: &VirtualMemoryMapEntry) {
        let VirtualMemoryMapEntry {
            mut virtual_address,
            physical_address: mut start_physical_address,
            size: requested_size,
            flags,
        } = entry;

        assert!(!self.page_map_l4.as_ptr().is_null());
        assert!(is_aligned(self.page_map_l4.0 as _, PAGE_4K));

        let (aligned_start, size, _) =
            align_range(virtual_address as _, *requested_size as _, PAGE_4K);
        let mut size = size as u64;
        virtual_address = aligned_start as _;

        if self.is_user {
            assert!(*flags & flags::PTE_USER != 0);
            assert!(get_l4(virtual_address) != KERNEL_L4_INDEX as u64);
            let end = virtual_address + size;
            assert!(end <= MAX_USER_VIRTUAL_ADDRESS as u64);
        }

        if let Some(start_physical_address) = start_physical_address.as_mut() {
            let (aligned_start, physical_size, _) =
                align_range(*start_physical_address as _, *requested_size as _, PAGE_4K);
            assert!(physical_size as u64 == size);
            *start_physical_address = aligned_start as _;
        }

        // keep track of current address and size
        let mut physical_address = start_physical_address;

        assert!(size > 0);

        eprintln!(
            "{} {:08X?}",
            MemSize(size),
            VirtualMemoryMapEntry {
                virtual_address: virtual_address as _,
                physical_address: physical_address as _,
                size,
                flags: *flags,
            }
        );

        while size > 0 {
            let current_physical_address = physical_address.unwrap_or_else(|| {
                virtual2physical(unsafe { physical_page_allocator::alloc_zeroed() as _ }) as _
            });
            eprintln!(
                "[!] Mapping {:p} to {:p}",
                virtual_address as *const u8, current_physical_address as *const u8
            );
            let page_map_l4_index = get_l4(virtual_address) as usize;
            let page_directory_pointer_index = get_l3(virtual_address) as usize;
            let page_directory_index = get_l2(virtual_address) as usize;
            let page_table_index = get_l1(virtual_address) as usize;

            // Level 4
            let page_map_l4_entry = &mut self.page_map_l4.as_mut().entries[page_map_l4_index];

            if *page_map_l4_entry & flags::PTE_PRESENT == 0 {
                let page_directory_pointer_table = PageDirectoryTablePtr::alloc_new();
                *page_map_l4_entry =
                    (page_directory_pointer_table.to_physical() & ADDR_MASK) | flags::PTE_PRESENT;
            }
            // add new flags if any
            *page_map_l4_entry |= flags;
            eprintln!(
                "L4[{}]: {:p} = {:x}",
                page_map_l4_index, page_map_l4_entry, *page_map_l4_entry
            );

            // Level 3
            let mut page_directory_pointer_table =
                PageDirectoryTablePtr::from_entry(*page_map_l4_entry);

            let page_directory_pointer_entry =
                &mut page_directory_pointer_table.as_mut().entries[page_directory_pointer_index];

            if *page_directory_pointer_entry & flags::PTE_PRESENT == 0 {
                let page_directory_table = PageDirectoryTablePtr::alloc_new();
                *page_directory_pointer_entry =
                    (page_directory_table.to_physical() & ADDR_MASK) | flags::PTE_PRESENT;
            }

            // add new flags
            *page_directory_pointer_entry |= flags;
            eprintln!(
                "L3[{}]: {:p} = {:x}",
                page_directory_pointer_index,
                page_directory_pointer_entry,
                *page_directory_pointer_entry
            );

            // Level 2
            let mut page_directory_table =
                PageDirectoryTablePtr::from_entry(*page_directory_pointer_entry);
            let page_directory_entry =
                &mut page_directory_table.as_mut().entries[page_directory_index];

            // here we have an intersection, if we can map a 2MB page, we will, otherwise we will map a 4K page
            // if we are providing the pages (the user didn't provide), then we can't use 2MB pages
            // let can_map_2mb_page = physical_address
            //     .map(|phy_addr| {
            //         is_aligned(phy_addr as _, PAGE_2M)
            //             && is_aligned(virtual_address as _, PAGE_2M)
            //             && size >= PAGE_2M as u64
            //     })
            //     .unwrap_or(false);
            // TODO: we have disabled 2MB as its not easy to unmap in the middle, all pages must be the sames

            let can_map_2mb_page = false;
            if can_map_2mb_page {
                // we already have an entry here
                if *page_directory_entry & flags::PTE_PRESENT != 0 {
                    // did we have a mapping here that lead to 4k pages?
                    // if so, we should free the physical page allocation for them
                    if *page_directory_entry & flags::PTE_HUGE_PAGE == 0 {
                        let page_table_ptr =
                            PageDirectoryTablePtr::from_entry(*page_directory_entry);

                        unsafe { page_table_ptr.free() };
                    }
                }

                // Level 1
                *page_directory_entry = (current_physical_address & ADDR_MASK)
                    | flags
                    | flags::PTE_PRESENT
                    | flags::PTE_HUGE_PAGE;

                eprintln!(
                    "L2[{}] huge: {:p} = {:x}",
                    page_directory_index, page_directory_entry, *page_directory_entry
                );

                size -= PAGE_2M as u64;
                // do not overflow the address
                if size == 0 {
                    break;
                }
                virtual_address += PAGE_2M as u64;
                if let Some(physical_address) = physical_address.as_mut() {
                    *physical_address += PAGE_2M as u64;
                }
            } else {
                // continue mapping 4K pages
                if *page_directory_entry & flags::PTE_PRESENT == 0 {
                    let page_table = PageDirectoryTablePtr::alloc_new();
                    *page_directory_entry =
                        (page_table.to_physical() & ADDR_MASK) | flags::PTE_PRESENT;
                }
                // add new flags
                *page_directory_entry |= flags;
                eprintln!(
                    "L2[{}]: {:p} = {:x}",
                    page_directory_index, page_directory_entry, *page_directory_entry
                );

                // Level 1
                let mut page_table = PageDirectoryTablePtr::from_entry(*page_directory_entry);
                let page_table_entry = &mut page_table.as_mut().entries[page_table_index];
                *page_table_entry =
                    (current_physical_address & ADDR_MASK) | flags | flags::PTE_PRESENT;
                eprintln!(
                    "L1[{}]: {:p} = {:x}",
                    page_table_index, page_table_entry, *page_table_entry
                );

                size -= PAGE_4K as u64;
                // do not overflow the address
                if size == 0 {
                    break;
                }
                virtual_address += PAGE_4K as u64;
                if let Some(physical_address) = physical_address.as_mut() {
                    *physical_address += PAGE_4K as u64;
                }
            }

            eprintln!();
        }
    }

    /// Removes mapping of a virtual entry, it will free it from physical memory if it was allocated
    pub fn unmap(&mut self, entry: &VirtualMemoryMapEntry, is_allocated: bool) {
        let VirtualMemoryMapEntry {
            mut virtual_address,
            physical_address,
            size,
            flags,
        } = entry;

        assert!(physical_address.is_none());

        // get the end before alignment
        let (aligned_start, size, _) = align_range(virtual_address as _, *size as _, PAGE_4K);
        let mut size = size as u64;
        virtual_address = aligned_start as _;

        assert!(size > 0);

        eprintln!(
            "{} {:08X?}",
            MemSize(size),
            VirtualMemoryMapEntry {
                virtual_address: virtual_address as _,
                physical_address: *physical_address,
                size,
                flags: *flags,
            }
        );

        while size > 0 {
            unsafe {
                cpu::invalidate_tlp(virtual_address as _);
            }

            let page_map_l4_index = get_l4(virtual_address) as usize;
            let page_directory_pointer_index = get_l3(virtual_address) as usize;
            let page_directory_index = get_l2(virtual_address) as usize;
            let page_table_index = get_l1(virtual_address) as usize;

            // Level 4
            let page_map_l4_entry = &mut self.page_map_l4.as_mut().entries[page_map_l4_index];

            if *page_map_l4_entry & flags::PTE_PRESENT == 0 {
                panic!("Trying to unmap a non-mapped address");
            }
            // remove flags
            *page_map_l4_entry &= !flags;
            eprintln!(
                "L4[{}]: {:p} = {:x}",
                page_map_l4_index, page_map_l4_entry, *page_map_l4_entry
            );

            // Level 3
            let mut page_directory_pointer_table =
                PageDirectoryTablePtr::from_entry(*page_map_l4_entry);

            let page_directory_pointer_entry =
                &mut page_directory_pointer_table.as_mut().entries[page_directory_pointer_index];

            if *page_directory_pointer_entry & flags::PTE_PRESENT == 0 {
                panic!("Trying to unmap a non-mapped address");
            }
            // remove flags
            *page_directory_pointer_entry &= !flags;
            eprintln!(
                "L3[{}]: {:p} = {:x}",
                page_directory_pointer_index,
                page_directory_pointer_entry,
                *page_directory_pointer_entry
            );

            // Level 2
            let mut page_directory_table =
                PageDirectoryTablePtr::from_entry(*page_directory_pointer_entry);
            let page_directory_entry =
                &mut page_directory_table.as_mut().entries[page_directory_index];

            if *page_directory_entry & flags::PTE_PRESENT == 0 {
                panic!("Trying to unmap a non-mapped address");
            }
            // remove flags
            *page_directory_entry &= !flags;

            // Level 1
            let mut page_table = PageDirectoryTablePtr::from_entry(*page_directory_entry);
            let page_table_entry = &mut page_table.as_mut().entries[page_table_index];
            if *page_table_entry & flags::PTE_PRESENT == 0 {
                panic!("Trying to unmap a non-mapped address");
            }
            let physical_entry = PageDirectoryTablePtr::from_entry(*page_table_entry);
            if is_allocated {
                unsafe { physical_entry.free() };
            }
            // remove whole entry
            *page_table_entry = 0;
            eprintln!(
                "L1[{}]: {:p} = {:x}",
                page_table_index, page_table_entry, *page_table_entry
            );

            size -= PAGE_4K as u64;
            // do not overflow the address
            if size == 0 {
                break;
            }
            virtual_address += PAGE_4K as u64;
        }
    }

    pub fn is_address_mapped(&self, addr: u64) -> bool {
        let page_map_l4_index = get_l4(addr) as usize;
        let page_directory_pointer_index = get_l3(addr) as usize;
        let page_directory_index = get_l2(addr) as usize;
        let page_table_index = get_l1(addr) as usize;

        // Level 4
        let page_map_l4 = self.page_map_l4.as_ref();
        let page_map_l4_entry = &page_map_l4.entries[page_map_l4_index];

        if *page_map_l4_entry & flags::PTE_PRESENT == 0 {
            return false;
        }
        eprintln!(
            "L4[{}]: {:p} = {:x}",
            page_map_l4_index, page_map_l4_entry, *page_map_l4_entry
        );

        // Level 3
        let page_directory_pointer_table = PageDirectoryTablePtr::from_entry(*page_map_l4_entry);
        let page_directory_pointer_entry =
            &page_directory_pointer_table.as_ref().entries[page_directory_pointer_index];
        if *page_directory_pointer_entry & flags::PTE_PRESENT == 0 {
            return false;
        }
        eprintln!(
            "L3[{}]: {:p} = {:x}",
            page_directory_pointer_index,
            page_directory_pointer_entry,
            *page_directory_pointer_entry
        );

        // Level 2
        let page_directory_table = PageDirectoryTablePtr::from_entry(*page_directory_pointer_entry);
        let page_directory_entry = &page_directory_table.as_ref().entries[page_directory_index];
        if *page_directory_entry & flags::PTE_PRESENT == 0 {
            return false;
        }
        if *page_directory_entry & flags::PTE_HUGE_PAGE != 0 {
            return true;
        }
        eprintln!(
            "L2[{}]: {:p} = {:x}",
            page_directory_index, page_directory_entry, *page_directory_entry
        );

        // Level 1
        let page_table = PageDirectoryTablePtr::from_entry(*page_directory_entry);
        let page_table_entry = &page_table.as_ref().entries[page_table_index];
        if *page_table_entry & flags::PTE_PRESENT == 0 {
            return false;
        }
        eprintln!(
            "L1[{}]: {:p} = {:x}",
            page_table_index, page_table_entry, *page_table_entry
        );

        true
    }

    // TODO: add tests for this
    fn do_for_ranges_enteries<R1, R2, F>(&mut self, l4_ranges: R1, l3_ranges: R2, mut f: F)
    where
        R1: RangeBounds<usize>,
        R2: RangeBounds<usize>,
        F: FnMut(&mut u64),
    {
        let page_map_l4 = self.page_map_l4.as_mut();

        let present = |entry: &&mut u64| **entry & flags::PTE_PRESENT != 0;

        fn as_page_directory_table_flat(entry: &mut u64) -> IterMut<u64> {
            let page_directory_table = PageDirectoryTablePtr::enteries_from_mut_entry(entry);
            page_directory_table.entries.iter_mut()
        }

        // handle 2MB pages and below
        let handle_2mb_pages = |page_directory_entry: &mut u64| {
            // handle 2MB pages
            if *page_directory_entry & flags::PTE_HUGE_PAGE != 0 {
                f(page_directory_entry);
            } else {
                as_page_directory_table_flat(page_directory_entry)
                    .filter(present)
                    .for_each(&mut f);
            }
        };

        let l4_start = match l4_ranges.start_bound() {
            core::ops::Bound::Included(&start) => start,
            core::ops::Bound::Unbounded => 0,
            core::ops::Bound::Excluded(_) => unreachable!("Excluded start bound"),
        };
        let l4_end = match l4_ranges.end_bound() {
            core::ops::Bound::Included(&end) => end,
            core::ops::Bound::Excluded(&end) => end - 1,
            core::ops::Bound::Unbounded => 0x1FF, // max entries
        };
        let l3_start = match l3_ranges.start_bound() {
            core::ops::Bound::Included(&start) => start,
            core::ops::Bound::Unbounded => 0,
            core::ops::Bound::Excluded(_) => unreachable!("Excluded start bound"),
        };
        let l3_end = match l3_ranges.end_bound() {
            core::ops::Bound::Included(&end) => end,
            core::ops::Bound::Excluded(&end) => end - 1,
            core::ops::Bound::Unbounded => 0x1FF, // max entries
        };

        let l4_skip = l4_start;
        let l4_take = l4_end - l4_skip + 1;
        let l3_skip = l3_start;
        let l3_take = l3_end - l3_skip + 1;

        page_map_l4
            .entries
            .iter_mut()
            .skip(l4_skip)
            .take(l4_take) //skip the kernel (the last one)
            .flat_map(as_page_directory_table_flat)
            .skip(l3_skip)
            .take(l3_take)
            .filter(present)
            .flat_map(as_page_directory_table_flat)
            .filter(present)
            .for_each(handle_2mb_pages);
    }

    // the handler function definition is `fn(page_entry: &mut u64)`
    fn do_for_every_user_entry(&mut self, f: impl FnMut(&mut u64)) {
        self.do_for_ranges_enteries(0..NUM_USER_L4_INDEXES, 0..=0x1FF, f)
    }

    // the handler function definition is `fn(page_entry: &mut u64)`
    fn do_for_kernel_process_entry(&mut self, f: impl FnMut(&mut u64)) {
        self.do_for_ranges_enteries(
            KERNEL_L4_INDEX..=KERNEL_L4_INDEX,
            KERNEL_L3_PROCESS_INDEX_START..=KERNEL_L3_PROCESS_INDEX_END,
            f,
        );
    }

    // search for all the pages that are mapped to the user ranges and unmap them and free their memory
    // also unmap any process specific kernel memory
    pub fn unmap_process_memory(&mut self) {
        let free_page = |entry: &mut u64| {
            assert!(
                *entry & flags::PTE_HUGE_PAGE == 0,
                "We haven't implemented 2MB physical pages for user allocation"
            );
            let page_table_ptr = PageDirectoryTablePtr::from_entry(*entry);
            unsafe { page_table_ptr.free() };
            *entry = 0;
        };

        self.do_for_every_user_entry(free_page);
        self.do_for_kernel_process_entry(free_page);
    }
}
