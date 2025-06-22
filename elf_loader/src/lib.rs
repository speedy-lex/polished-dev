//! ELF Loader Library
//!
//! This library provides functionality to load and execute a kernel or application from an ELF (Executable and Linkable Format) file.
//! It is designed for use in a UEFI bootloader context, where the kernel is loaded from disk and executed directly from its entry point address.
//!
//! # Overview
//!
//! - Reads an ELF file from disk (e.g., from an EFI system partition)
//! - Parses the ELF file and loads its segments into memory at the addresses specified by the ELF headers
//! - Allocates memory using UEFI services, respecting the segment permissions
//! - Copies segment data from the file into memory, zero-filling any uninitialized data (BSS)
//! - Returns the entry point address and a callable function pointer to start the loaded kernel
//!
//! # Usage
//!
//! Call [`load_kernel`] with the path to the ELF file. The function returns the entry point address and a function pointer you can call to transfer control to the loaded kernel.
//!
//! # Safety
//!
//! This code uses unsafe operations to copy memory and to transmute the entry point address into a function pointer. Ensure the ELF file is trusted and valid.
//!
//! # ELF Entry Point
//!
//! The entry point of an ELF file is a special address specified in the ELF header (the `e_entry` field). When the loader finishes loading all segments, it transfers control to this address to start execution of the program or kernel.
//!
//! ## How the Entry Point is Set
//!
//! - In Rust, you typically define the entry point function (e.g., `fn _start()`) and mark it with `#[no_mangle]` to prevent the compiler from renaming it.
//! - The linker script (e.g., `ENTRY(_start)`) tells the linker to set the ELF header's entry point to the address of your `_start` function.
//!
//! When this loader returns the entry point address and function pointer, it is using the value from the ELF header, which should match your `#[no_mangle]` entry function as set by your linker script.
//!
//! For more details, see the documentation for your linker and the `ENTRY()` directive in your linker script.

#![no_std]

#[cfg(feature = "uefi")]
use core::slice;

#[cfg(feature = "uefi")]
use polished_files::uefi::read_file;
#[cfg(feature = "uefi")]
use x86_64::{structures::paging::{FrameAllocator, OffsetPageTable, Size4KiB}, PhysAddr};
#[cfg(feature = "uefi")]
use xmas_elf::{ElfFile, program};

#[cfg(feature = "uefi")]
/// Loads a kernel from the specified ELF file path.
///
/// # Arguments
///
/// * `file_path` - The path to the ELF file to load (e.g., "\\EFI\\BOOT\\kernel").
///
/// # Returns
///
/// A tuple containing:
/// - The entry point address of the loaded kernel (as `usize`)
/// - A function pointer to the kernel's entry point (as `unsafe extern "C" fn() -> !`)
///
/// # How it works
///
/// 1. Reads the ELF file from disk into memory.
/// 2. Parses the ELF file and iterates over its program headers.
/// 3. For each loadable segment, allocates memory at the address requested by the ELF file.
/// 4. Copies the segment data from the file into the allocated memory, zero-filling any extra space (for BSS).
/// 5. Returns the entry point address and a function pointer to the entry point.
///
/// # Safety
///
/// The returned function pointer is only valid if the ELF file is well-formed and the memory was allocated and loaded correctly.
///
/// # Example
///
/// ```ignore
/// let (entry, kernel_entry) = load_kernel("\\EFI\\BOOT\\kernel");
/// // To start the kernel:
/// unsafe { kernel_entry() };
/// ```
pub fn load_kernel(
    file_path: &str,
    page_table: &mut OffsetPageTable,
    frame_alloc: &mut impl FrameAllocator<Size4KiB>,
    max_phys_addr: PhysAddr,
    offset_phys_addr: Option<PhysAddr>,
) -> unsafe extern "C" fn() -> ! {
    use x86_64::{structures::paging::{PageTable, OffsetPageTable}, VirtAddr};
    let mut new_page_table_ptr: Option<*mut PageTable> = None;
    if let Some(offset) = offset_phys_addr {
        // Redo paging, identity mapping won't work
        // SAFETY: frame_alloc must be a BumpFrameAllocator
        let bump_alloc = unsafe {
            &mut *(frame_alloc as *mut _ as *mut polished_allocators::frame::BumpFrameAllocator)
        };
        new_page_table_ptr = Some(setup_paging_with_offset(offset, max_phys_addr, bump_alloc));
    } else {
        log::info!("identity mapping should work, skipping page table refactor")
    }

    // If we switched page tables, update the OffsetPageTable to use the new PML4
    let mut local_page_table;
    let page_table = if let Some(pml4_ptr) = new_page_table_ptr {
        local_page_table = unsafe { OffsetPageTable::new(&mut *pml4_ptr, VirtAddr::new(offset_phys_addr.unwrap().as_u64())) };
        &mut local_page_table
    } else {
        page_table
    };

    // Log the file path being loaded
    log::info!("Loading kernel from ELF file: {file_path}");
    // Read the entire ELF file into memory
    let bytes = read_file(file_path).unwrap();
    // Parse the ELF file structure
    let elf = ElfFile::new(&bytes).expect("Failed to parse ELF file");

    // Iterate over each program header (segment) in the ELF file
    for ph in elf.program_iter() {
        use x86_64::{structures::paging::{Page, PageTableFlags, Size4KiB}, VirtAddr};

        let ph_type = ph.get_type().ok();
        log::info!("Found program header: {ph_type:?}");
        // Skip dynamic segments (not needed for kernel loading)
        if ph_type == Some(program::Type::Dynamic) {
            log::warn!("Skipping dynamic segment");
        }
        // Only process loadable segments
        if ph_type != Some(program::Type::Load) {
            continue;
        }

        // Get segment file offset, size in file, size in memory, and virtual address
        let file_offset = ph.offset() as usize;
        let file_size = ph.file_size() as usize;
        let mem_size = ph.mem_size() as usize;
        let virt_addr = VirtAddr::new(ph.virtual_addr());

        // Calculate page flags
        let mut flags = PageTableFlags::PRESENT;
        if !ph.flags().is_execute() {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        if ph.flags().is_write() {
            flags |= PageTableFlags::WRITABLE;
        }

        log::info!(
            "Loading segment: file_offset=0x{file_offset:x}, file_size=0x{file_size:x}, mem_size=0x{mem_size:x}, virt_addr=0x{virt_addr:x}"
        );

        // Calculate the page range
        let page_start = Page::<Size4KiB>::containing_address(virt_addr);
        let page_end = Page::<Size4KiB>::containing_address(virt_addr + mem_size as u64);
        let page_range = page_start..=page_end;

        log::info!(
            "Allocating pages {page_range:?}"
        );

        // Allocate memory at the requested virtual address
        for page in page_range {
            use x86_64::structures::paging::Mapper;
            let frame = frame_alloc.allocate_frame().unwrap();
            unsafe {
                page_table.map_to(page, frame, flags, frame_alloc)
            }.unwrap().flush();
        }

        let segment = unsafe {
            slice::from_raw_parts_mut(virt_addr.as_mut_ptr(), mem_size)
        };

        // Copy segment data from the ELF file into the allocated memory
        segment[..file_size].copy_from_slice(&bytes[file_offset..file_offset + file_size]);

        // Zero-fill any remaining memory (for .bss or uninitialized data)
        if mem_size > file_size {
            segment[file_size..].fill(0);
        }

        log::info!("Segment loaded at 0x{virt_addr:x}");
    }

    // Get the entry point address from the ELF header
    let entry_point = elf.header.pt2.entry_point() as usize;
    log::info!("Kernel entry point: 0x{entry_point:x}");
    // Convert the entry point address to a function pointer
    let kernel_entry: unsafe extern "C" fn() -> ! = unsafe { core::mem::transmute(entry_point) };

    kernel_entry
}

#[cfg(feature = "uefi")]
pub fn setup_paging_with_offset(
    offset_phys_addr: PhysAddr,
    max_phys_addr: PhysAddr,
    frame_alloc: &mut polished_allocators::frame::BumpFrameAllocator,
) -> *mut x86_64::structures::paging::PageTable {
    use x86_64::PhysAddr;
    use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
    // Helper to allocate a zeroed page table
    fn alloc_table(frame_alloc: &mut polished_allocators::frame::BumpFrameAllocator) -> &'static mut PageTable {
        let frame = frame_alloc.allocate_frame().expect("Failed to allocate frame for page table");
        let ptr = frame.start_address().as_u64() as *mut PageTable;
        unsafe {
            ptr.write(PageTable::new());
            &mut *ptr
        }
    }
    // Helper to map a single 4KiB page
    fn map_page(
        pml4: &mut PageTable,
        virt: u64,
        phys: u64,
        flags: PageTableFlags,
        frame_alloc: &mut polished_allocators::frame::BumpFrameAllocator,
    ) {
        let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
        let pd_idx   = ((virt >> 21) & 0x1FF) as usize;
        let pt_idx   = ((virt >> 12) & 0x1FF) as usize;
        // PDPT
        let pdpt = if pml4[pml4_idx].is_unused() {
            let pdpt = alloc_table(frame_alloc);
            pml4[pml4_idx].set_addr(PhysAddr::new(pdpt as *const _ as u64), PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
            pdpt
        } else {
            unsafe { &mut *(pml4[pml4_idx].addr().as_u64() as *mut PageTable) }
        };
        // PD
        let pd = if pdpt[pdpt_idx].is_unused() {
            let pd = alloc_table(frame_alloc);
            pdpt[pdpt_idx].set_addr(PhysAddr::new(pd as *const _ as u64), PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
            pd
        } else {
            unsafe { &mut *(pdpt[pdpt_idx].addr().as_u64() as *mut PageTable) }
        };
        // PT
        let pt = if pd[pd_idx].is_unused() {
            let pt = alloc_table(frame_alloc);
            pd[pd_idx].set_addr(PhysAddr::new(pt as *const _ as u64), PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
            pt
        } else {
            unsafe { &mut *(pd[pd_idx].addr().as_u64() as *mut PageTable) }
        };
        // Set the page table entry
        pt[pt_idx].set_addr(PhysAddr::new(phys), flags | PageTableFlags::PRESENT);
    }
    // Allocate root PML4
    let pml4 = alloc_table(frame_alloc);

    // Use offset_phys_addr as the kernel virtual base and map physical memory up to max_phys_addr
    let kernel_virt_base = offset_phys_addr.as_u64();
    let kernel_phys_base = 0x00100000u64;
    let kernel_phys_end = max_phys_addr.as_u64();
    let kernel_size = kernel_phys_end.saturating_sub(kernel_phys_base);
    let num_pages = kernel_size.div_ceil(0x1000);
    for i in 0..num_pages {
        let virt = kernel_virt_base + i * 0x1000;
        let phys = kernel_phys_base + i * 0x1000;
        map_page(
            pml4,
            virt,
            phys,
            PageTableFlags::WRITABLE,
            frame_alloc,
        );
    }
    // (Optional) Identity map a trampoline region (first 2 MiB)
    for i in 0..512 {
        let addr = i * 0x1000;
        map_page(
            pml4,
            addr as u64,
            addr as u64,
            PageTableFlags::WRITABLE,
            frame_alloc,
        );
    }
    // Map the current stack (assume stack is in low memory, identity map)
    let stack_start = 0x70000u64; // Example: 448 KiB
    let stack_end = 0x80000u64;   // Example: 512 KiB
    let mut addr = stack_start;
    while addr < stack_end {
        map_page(
            pml4,
            addr,
            addr,
            PageTableFlags::WRITABLE,
            frame_alloc,
        );
        addr += 0x1000;
    }
    // Write new PML4 physical address to CR3
    use x86_64::registers::control::Cr3;
    let pml4_phys = PhysAddr::new(pml4 as *const _ as u64);
    let pml4_frame = PhysFrame::containing_address(pml4_phys);
    unsafe {
        Cr3::write(pml4_frame, Cr3::read().1);
    }
    pml4 as *mut PageTable
}
