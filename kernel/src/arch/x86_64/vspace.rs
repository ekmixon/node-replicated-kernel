use core::fmt;
use core::mem::transmute;
use core::pin::Pin;

use alloc::boxed::Box;
use alloc::string::String;

use kpi::SystemCallError;
use x86::bits64::paging::*;
use x86::controlregs;

use crate::alloc::string::ToString;
use crate::memory::vspace::{AddressSpaceError, MapAction, ResourceType};
use crate::memory::{kernel_vaddr_to_paddr, paddr_to_kernel_vaddr, Frame, PAddr, VAddr};

use super::kcb::get_kcb;

pub struct VSpace {
    pub pml4: Pin<Box<PML4>>,
}

impl Drop for VSpace {
    fn drop(&mut self) {
        //panic!("Drop for VSpace!");
    }
}

impl VSpace {
    /// Create a new address-space.
    ///
    /// Allocate an initial PML4 table for it.
    pub fn new() -> VSpace {
        VSpace {
            pml4: Box::pin(
                [PML4Entry::new(PAddr::from(0x0u64), PML4Flags::empty()); PAGE_SIZE_ENTRIES],
            ),
        }
    }

    pub fn pml4_address(&self) -> PAddr {
        let pml4_vaddr = VAddr::from(&*self.pml4 as *const _ as u64);
        kernel_vaddr_to_paddr(pml4_vaddr)
    }

    /// Constructs an identity map but with an offset added to the region.
    ///
    /// # Example
    /// `map_identity_with_offset(0x20000, 0x1000, 0x2000, ReadWriteKernel)`
    /// will set the virtual addresses at 0x21000 -- 0x22000 to
    /// point to physical 0x1000 - 0x2000.
    pub(crate) fn map_identity_with_offset(
        &mut self,
        at_offset: PAddr,
        pbase: PAddr,
        end: PAddr,
        rights: MapAction,
    ) -> Result<(), AddressSpaceError> {
        // TODO: maybe better to provide a length instead of end
        // so harder for things to break
        assert!(end > pbase, "End should be bigger than pbase");

        let vbase = VAddr::from_u64((at_offset + pbase).as_u64());
        let size = (end - pbase).as_usize();
        debug!(
            "map_identity_with_offset {:#x} -- {:#x} -> {:#x} -- {:#x}",
            vbase,
            vbase + size,
            pbase,
            pbase + size
        );
        let kcb = crate::kcb::get_kcb();
        let mut pmanager = kcb.mem_manager();

        self.map_generic(vbase, (pbase, size), rights, &mut pmanager)
    }

    /// Constructs an identity map in this region of memory.
    ///
    /// # Example
    /// `map_identity(0x2000, 0x3000)` will map everything between 0x2000 and 0x3000 to
    /// physical address 0x2000 -- 0x3000.
    pub(crate) fn map_identity(&mut self, base: PAddr, end: PAddr, rights: MapAction) {
        self.map_identity_with_offset(PAddr::from(0x0), base, end, rights)
            .expect("Can't identity map region");
    }

    /// A pretty generic map function, it puts the physical memory range `pregion` with base and
    /// size into the virtual base at address `vbase`.
    ///
    /// The algorithm tries to allocate the biggest page-sizes possible for the allocations.
    /// We require that `vbase` and `pregion` values are all aligned to a page-size.
    /// TODO: We panic in case there is already a mapping covering the region (should return error).
    pub(crate) fn map_generic(
        &mut self,
        vbase: VAddr,
        pregion: (PAddr, usize),
        rights: MapAction,
        pager: &mut crate::memory::tcache::TCache,
    ) -> Result<(), AddressSpaceError> {
        let (pbase, psize) = pregion;
        assert_eq!(pbase % BASE_PAGE_SIZE, 0);
        assert_eq!(psize % BASE_PAGE_SIZE, 0);
        assert_eq!(vbase % BASE_PAGE_SIZE, 0);
        assert_ne!(rights, MapAction::None, "TODO: Should we allow that?");

        debug!(
            "map_generic {:#x} -- {:#x} -> {:#x} -- {:#x} {}",
            vbase,
            vbase + psize,
            pbase,
            pbase + psize,
            rights
        );

        let pml4_idx = pml4_index(vbase);
        if !self.pml4[pml4_idx].is_present() {
            trace!("New PDPDT for {:?} @ PML4[{}]", vbase, pml4_idx);
            self.pml4[pml4_idx] = self.new_pdpt(pager);
        }
        assert!(
            self.pml4[pml4_idx].is_present(),
            "The PML4 slot we need was not allocated?"
        );

        let pdpt = self.get_pdpt(self.pml4[pml4_idx]);
        let mut pdpt_idx = pdpt_index(vbase);
        // TODO: if we support None mappings, this is if not good enough:
        if !pdpt[pdpt_idx].is_present() {
            // The virtual address corresponding to our position within the page-table
            let vaddr_pos: usize = PML4_SLOT_SIZE * pml4_idx + HUGE_PAGE_SIZE * pdpt_idx;

            // In case we can map something at a 1 GiB granularity and
            // we still have at least 1 GiB to map, create huge-page mappings
            if vbase.as_usize() == vaddr_pos
                && (pbase % HUGE_PAGE_SIZE == 0)
                && psize >= HUGE_PAGE_SIZE
            {
                // To track how much space we've covered
                let mut mapped = 0;

                // Add entries to PDPT as long as we're within this allocated PDPT table
                // and have 1 GiB chunks to map:
                while mapped < psize && ((psize - mapped) >= HUGE_PAGE_SIZE) && pdpt_idx < 512 {
                    assert!(!pdpt[pdpt_idx].is_present());
                    pdpt[pdpt_idx] = PDPTEntry::new(
                        pbase + mapped,
                        PDPTFlags::P | PDPTFlags::PS | rights.to_pdpt_rights(),
                    );
                    trace!(
                        "Mapped 1GiB range {:#x} -- {:#x} -> {:#x} -- {:#x}",
                        vbase + mapped,
                        (vbase + mapped) + HUGE_PAGE_SIZE,
                        pbase + mapped,
                        (vbase + mapped) + HUGE_PAGE_SIZE
                    );

                    pdpt_idx += 1;
                    mapped += HUGE_PAGE_SIZE;
                }

                if mapped < psize {
                    trace!(
                        "map_generic recurse from 1 GiB map to finish {:#x} -- {:#x} -> {:#x} -- {:#x}",
                        vbase + mapped,
                        vbase + (psize - mapped),
                        (pbase + mapped),
                        pbase + (psize - mapped),
                    );
                    return self.map_generic(
                        vbase + mapped,
                        ((pbase + mapped), psize - mapped),
                        rights,
                        pager,
                    );
                } else {
                    // Everything fit in 1 GiB ranges,
                    // We're done with mappings
                    return Ok(());
                }
            } else {
                trace!(
                    "Mapping 0x{:x} -- 0x{:x} is smaller than 1 GiB, going deeper.",
                    vbase,
                    vbase + psize
                );
                pdpt[pdpt_idx] = self.new_pd(pager);
            }
        }
        assert!(
            pdpt[pdpt_idx].is_present(),
            "The PDPT entry we're relying on is not allocated?"
        );
        assert!(
            !pdpt[pdpt_idx].is_page(),
            "An existing mapping already covers the 1 GiB range we're trying to map in?"
        );

        let pd = self.get_pd(pdpt[pdpt_idx]);
        let mut pd_idx = pd_index(vbase);
        if !pd[pd_idx].is_present() {
            let vaddr_pos: usize =
                PML4_SLOT_SIZE * pml4_idx + HUGE_PAGE_SIZE * pdpt_idx + LARGE_PAGE_SIZE * pd_idx;

            // In case we can map something at a 2 MiB granularity and
            // we still have at least 2 MiB to map create large-page mappings
            if vbase.as_usize() == vaddr_pos
                && (pbase % LARGE_PAGE_SIZE == 0)
                && psize >= LARGE_PAGE_SIZE
            {
                let mut mapped = 0;
                // Add entries as long as we are within this allocated PDPT table
                // and have at least 2 MiB things to map
                while mapped < psize && ((psize - mapped) >= LARGE_PAGE_SIZE) && pd_idx < 512 {
                    if pd[pd_idx].is_present() {
                        panic!("Already mapped pd at {:#x}", pbase + mapped);
                    }

                    pd[pd_idx] = PDEntry::new(
                        pbase + mapped,
                        PDFlags::P | PDFlags::PS | rights.to_pd_rights(),
                    );
                    trace!(
                        "Mapped 2 MiB region {:#x} -- {:#x} -> {:#x} -- {:#x}",
                        vbase + mapped,
                        (vbase + mapped) + LARGE_PAGE_SIZE,
                        pbase + mapped,
                        (pbase + mapped) + LARGE_PAGE_SIZE
                    );

                    pd_idx += 1;
                    mapped += LARGE_PAGE_SIZE;
                }

                if mapped < psize {
                    trace!(
                        "map_generic recurse from 2 MiB map to finish {:#x} -- {:#x} -> {:#x} -- {:#x}",
                        vbase + mapped,
                        vbase + (psize - mapped),
                        (pbase + mapped),
                        pbase + (psize - mapped),
                    );
                    return self.map_generic(
                        vbase + mapped,
                        ((pbase + mapped), psize - mapped),
                        rights,
                        pager,
                    );
                } else {
                    // Everything fit in 2 MiB ranges,
                    // We're done with mappings
                    return Ok(());
                }
            } else {
                trace!(
                    "Mapping 0x{:x} -- 0x{:x} is smaller than 2 MiB, going deeper.",
                    vbase,
                    vbase + psize
                );
                pd[pd_idx] = self.new_pt(pager);
            }
        }
        assert!(
            pd[pd_idx].is_present(),
            "The PD entry we're relying on is not allocated?"
        );
        assert!(
            !pd[pd_idx].is_page(),
            "An existing mapping already covers the 2 MiB range we're trying to map in?"
        );

        let pt = self.get_pt(pd[pd_idx]);
        let mut pt_idx = pt_index(vbase);
        let mut mapped: usize = 0;
        while mapped < psize && pt_idx < 512 {
            if !pt[pt_idx].is_present() {
                pt[pt_idx] = PTEntry::new(pbase + mapped, PTFlags::P | rights.to_pt_rights());
            } else {
                assert!(
                    pt[pt_idx].is_present(),
                    "An existing mapping already covers the 4 KiB range we're trying to map?"
                );
            }

            mapped += BASE_PAGE_SIZE;
            pt_idx += 1;
        }

        // Need go to different PD/PDPT/PML4 slot
        if mapped < psize {
            trace!(
                "map_generic recurse from 4 KiB map to finish {:#x} -- {:#x} -> {:#x} -- {:#x}",
                vbase + mapped,
                vbase + (psize - mapped),
                (pbase + mapped),
                pbase + (psize - mapped),
            );
            return self.map_generic(
                vbase + mapped,
                ((pbase + mapped), psize - mapped),
                rights,
                pager,
            );
        } else {
            // else we're done here, return
            Ok(())
        }
    }

    fn new_pt(&self, pager: &mut dyn crate::memory::PhysicalPageProvider) -> PDEntry {
        let mut frame: Frame = pager.allocate_base_page().expect("Allocation must work");
        unsafe { frame.zero() };
        return PDEntry::new(frame.base, PDFlags::P | PDFlags::RW | PDFlags::US);
    }

    fn new_pd(&self, pager: &mut dyn crate::memory::PhysicalPageProvider) -> PDPTEntry {
        let mut frame: Frame = pager.allocate_base_page().expect("Allocation must work");
        unsafe { frame.zero() };
        return PDPTEntry::new(frame.base, PDPTFlags::P | PDPTFlags::RW | PDPTFlags::US);
    }

    fn new_pdpt(&self, pager: &mut dyn crate::memory::PhysicalPageProvider) -> PML4Entry {
        let mut frame: Frame = pager.allocate_base_page().expect("Allocation must work");
        unsafe { frame.zero() };
        return PML4Entry::new(frame.base, PML4Flags::P | PML4Flags::RW | PML4Flags::US);
    }

    /// Resolve a PDEntry to a page table.
    fn get_pt<'b>(&self, entry: PDEntry) -> &'b mut PT {
        unsafe { transmute::<VAddr, &mut PT>(paddr_to_kernel_vaddr(entry.address())) }
    }

    /// Resolve a PDPTEntry to a page directory.
    fn get_pd<'b>(&self, entry: PDPTEntry) -> &'b mut PD {
        unsafe { transmute::<VAddr, &mut PD>(paddr_to_kernel_vaddr(entry.address())) }
    }

    /// Resolve a PML4Entry to a PDPT.
    fn get_pdpt<'b>(&self, entry: PML4Entry) -> &'b mut PDPT {
        unsafe { transmute::<VAddr, &mut PDPT>(paddr_to_kernel_vaddr(entry.address())) }
    }

    pub(crate) fn resolve_addr(&self, addr: VAddr) -> Option<PAddr> {
        let pml4_idx = pml4_index(addr);
        if self.pml4[pml4_idx].is_present() {
            let pdpt_idx = pdpt_index(addr);
            let pdpt = self.get_pdpt(self.pml4[pml4_idx]);
            if pdpt[pdpt_idx].is_present() {
                if pdpt[pdpt_idx].is_page() {
                    // Page is a 1 GiB mapping, we have to return here
                    let page_offset = addr.huge_page_offset();
                    return Some(pdpt[pdpt_idx].address() + page_offset);
                } else {
                    let pd_idx = pd_index(addr);
                    let pd = self.get_pd(pdpt[pdpt_idx]);
                    if pd[pd_idx].is_present() {
                        if pd[pd_idx].is_page() {
                            // Encountered a 2 MiB mapping, we have to return here
                            let page_offset = addr.large_page_offset();
                            return Some(pd[pd_idx].address() + page_offset);
                        } else {
                            let pt_idx = pt_index(addr);
                            let pt = self.get_pt(pd[pd_idx]);
                            if pt[pt_idx].is_present() {
                                let page_offset = addr.base_page_offset();
                                return Some(pt[pt_idx].address() + page_offset);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Take ownership of a list of frames and map them in our address space
    /// at `base`.
    pub fn map_frames(
        &mut self,
        base: VAddr,
        frames: Vec<(Frame, MapAction)>,
        pager: &mut crate::memory::tcache::TCache,
    ) -> Result<(), AddressSpaceError> {
        assert!(frames.len() > 0);
        assert_eq!(
            base % frames[0].0.size(),
            0,
            "First frame should be aligned to size of frame (large page at 2 MiB offset)"
        );

        let mut current_base = base;
        for (frame, rights) in frames.into_iter() {
            self.map_frame(current_base, frame, rights, pager)?;
            current_base += frame.size();
        }

        Ok(())
    }

    /// New API replaces map()
    pub(crate) fn map_frame(
        &mut self,
        base: VAddr,
        frame: Frame,
        action: MapAction,
        pager: &mut crate::memory::tcache::TCache,
    ) -> Result<(), AddressSpaceError> {
        self.map_generic(base, (frame.base, frame.size()), action, pager);
        Ok(())
        // TODO(metadata) add frame to vspace
    }

    /// Back a region of virtual address space with
    /// allocated physical memory (that got aligned to `palignment`).
    ///
    ///  * The base should be a multiple of `BASE_PAGE_SIZE`.
    ///  * The size should be a multiple of `BASE_PAGE_SIZE`.
    ///
    /// TODO(broken): Remove this
    #[allow(unused)]
    pub fn map(
        &mut self,
        base: VAddr,
        size: usize,
        rights: MapAction,
        palignment: u64,
    ) -> Result<(PAddr, usize), AddressSpaceError> {
        assert_eq!(base % BASE_PAGE_SIZE, 0, "base is not page-aligned");
        assert_eq!(size % BASE_PAGE_SIZE, 0, "size is not page-aligned");
        let paddr =
            VSpace::allocate_pages_aligned(size / BASE_PAGE_SIZE, ResourceType::Memory, palignment);

        let kcb = crate::kcb::get_kcb();
        let mut pmanager = kcb.mem_manager();
        self.map_generic(base, (paddr, size), rights, &mut pmanager)?;
        Ok((paddr, size))
    }

    /// Does an allocation of physical memory where the base-address is a multiple of `align_to`.
    /// TODO(broken): Remove this
    pub(crate) fn allocate_pages_aligned(
        how_many: usize,
        typ: ResourceType,
        align_to: u64,
    ) -> PAddr {
        assert!(align_to.is_power_of_two(), "Alignment needs to be pow2");
        assert!(
            align_to >= BASE_PAGE_SIZE as u64,
            "Alignment needs to be at least page-size"
        );

        let alignment_mask = align_to - 1;
        let actual_how_many = how_many + ((align_to as usize) >> BASE_PAGE_SHIFT);
        assert!(actual_how_many >= how_many);

        // The region we allocated
        let paddr = VSpace::allocate_pages(actual_how_many, typ);
        let end = paddr + (actual_how_many * BASE_PAGE_SIZE);

        // The region within the allocated one we actually want
        let aligned_paddr = PAddr::from((paddr + alignment_mask) & !alignment_mask);
        assert_eq!(aligned_paddr % align_to, 0, "Not aligned properly");
        let aligned_end = aligned_paddr + (how_many * BASE_PAGE_SIZE);

        // How many pages at the bottom and top we need to free
        let unaligned_unused_pages_bottom = (aligned_paddr - paddr).as_usize() / BASE_PAGE_SIZE;
        let unaligned_unused_pages_top = (end - aligned_end).as_usize() / BASE_PAGE_SIZE;

        debug!(
                "Wanted to allocate {} pages but we allocated {} ({:#x} -- {:#x}), keeping range ({:#x} -- {:#x}), freeing #pages at bottom {} and top {}",
                how_many, actual_how_many,
                paddr,
                end,
                aligned_paddr,
                aligned_paddr + (how_many * BASE_PAGE_SIZE),
                unaligned_unused_pages_bottom,
                unaligned_unused_pages_top
            );

        assert!(
            unaligned_unused_pages_bottom + unaligned_unused_pages_top
                == actual_how_many - how_many,
            "Don't loose any pages"
        );

        // Free unused top and bottom regions again:
        trace!("NYI free top");
        trace!("NYI free bottom");

        PAddr::from(aligned_paddr)
    }

    /// Allocates a set of consecutive physical pages, using UEFI.
    ///
    /// Zeroes the memory we allocate (TODO: I'm not sure if this is already done by UEFI).
    /// Returns a `u64` containing the base to that.
    ///
    /// TODO(broken): remove it!
    pub(crate) fn allocate_pages(how_many: usize, _typ: ResourceType) -> PAddr {
        let new_region: *mut u8 = unsafe {
            alloc::alloc::alloc_zeroed(core::alloc::Layout::from_size_align_unchecked(
                how_many * BASE_PAGE_SIZE,
                4096,
            ))
        };
        assert!(!new_region.is_null());

        kernel_vaddr_to_paddr(VAddr::from(new_region as usize))
    }
}

pub unsafe fn dump_current_table(log_level: usize) {
    let cr_three: u64 = controlregs::cr3();
    let pml4: PAddr = PAddr::from(cr_three);
    let pml4_table = transmute::<VAddr, &PML4>(paddr_to_kernel_vaddr(pml4));

    dump_table(pml4_table, log_level);
}

pub unsafe fn dump_table(pml4_table: &PML4, log_level: usize) {
    for (pml_idx, pml_item) in pml4_table.iter().enumerate() {
        if pml_item.is_present() {
            info!("PML4 item#{}: maps to {:?}", pml_idx, pml_item);

            let pdpt_table =
                transmute::<VAddr, &mut PDPT>(VAddr::from_u64(pml_item.address().as_u64()));
            if log_level <= 1 {
                continue;
            }

            for (pdpt_idx, pdpt_item) in pdpt_table.iter().enumerate() {
                info!("PDPT item#{}: maps to {:?}", pdpt_idx, pdpt_item);

                if pdpt_item.is_present() {
                    let pd_table =
                        transmute::<VAddr, &mut PD>(VAddr::from_u64(pdpt_item.address().as_u64()));
                    if pdpt_item.is_page() {
                        let vaddr: usize = (512 * (512 * (512 * 0x1000))) * pml_idx
                            + (512 * (512 * 0x1000)) * pdpt_idx;

                        info!("PDPT item: vaddr 0x{:x} maps to {:?}", vaddr, pdpt_item);
                    } else {
                        for (pd_idx, pd_item) in pd_table.iter().enumerate() {
                            info!("PD item#{}: maps to {:?}", pd_idx, pd_item);

                            if pd_item.is_present() {
                                let ptes = transmute::<VAddr, &mut PT>(VAddr::from_u64(
                                    pd_item.address().as_u64(),
                                ));

                                if pd_item.is_page() {
                                    let vaddr: usize = (512 * (512 * (512 * 0x1000))) * pml_idx
                                        + (512 * (512 * 0x1000)) * pdpt_idx
                                        + (512 * 0x1000) * pd_idx;

                                    info!("PD item: vaddr 0x{:x} maps to {:?}", vaddr, pd_item);
                                } else {
                                    assert!(!pd_item.is_page());
                                    for (pte_idx, pte) in ptes.iter().enumerate() {
                                        let vaddr: usize = (512 * (512 * (512 * 0x1000))) * pml_idx
                                            + (512 * (512 * 0x1000)) * pdpt_idx
                                            + (512 * 0x1000) * pd_idx
                                            + (0x1000) * pte_idx;

                                        if pte.is_present() {
                                            info!(
                                                "PT item: vaddr 0x{:x} maps to flags {:?}",
                                                vaddr, pte
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

use crate::graphviz as dot;
use alloc::format;
use alloc::vec::Vec;

#[derive(Copy, Clone)]
pub enum Nd<'a> {
    HugePage(PAddr),
    LargePage(PAddr),
    Page(PAddr),
    PT(&'a PT, Option<usize>),
    PD(&'a PD, Option<usize>),
    PDPT(&'a PDPT, Option<usize>),
    PML4(Pin<&'a PML4>, Option<usize>),
}

/// Edge is connection of two nodes and slot within the page-table.
type Ed<'a> = ((Nd<'a>, usize), (Nd<'a>, usize));

impl<'a> dot::Labeller<'a> for VSpace {
    type Node = Nd<'a>;
    type Edge = Ed<'a>;

    fn graph_id(&'a self) -> dot::Id<'a> {
        dot::Id::new("vspace").unwrap()
    }

    fn node_shape(&'a self, n: &Self::Node) -> Option<dot::LabelText<'a>> {
        match n {
            Nd::PT(pt, _) => Some(dot::LabelText::label("record")),
            Nd::PD(pd, _) => Some(dot::LabelText::label("record")),
            Nd::PDPT(pdpt, _) => Some(dot::LabelText::label("record")),
            Nd::PML4(pml4, _) => Some(dot::LabelText::label("record")),
            Nd::Page(addr) => None,
            Nd::LargePage(addr) => None,
            Nd::HugePage(addr) => None,
        }
    }

    /*
    /// Generate edge that look like this
    ///
    /// ```no-run
    /// "node4":f1 -> "node6":f0 [
    ///     id = 6
    /// ];
    /// ```
    fn edge_label(&'a self, e: &Self::Edge) {
        //dot::Id::new(label).expect("Cant make label")
    }*/

    /// Generate a label that looks like this:
    /// `<f0> PML4_0x400035c00008 | <f1> PML4_0x400035c000016 | (nil) | ... | (nil) `
    fn node_label(&'a self, n: &Self::Node) -> dot::LabelText<'a> {
        let mut node_label = String::with_capacity(512 * 8);

        enum Printer {
            EmitLine,
            EmitDots,
            Skip,
        }

        let label = match n {
            Nd::PT(pt, _) => {
                let mut state = Printer::EmitLine;
                for pt_idx in 0..pt.len() {
                    if pt_idx == 511 {
                        state = Printer::EmitLine;
                    }
                    let pt_item = pt[pt_idx];

                    match state {
                        Printer::EmitLine => {
                            if pt_item.is_present() {
                                if node_label.len() > 0 {
                                    node_label += " | "
                                }
                                node_label +=
                                    format!("<f{}> {:#x}", pt_idx, pt_item.address(),).as_str();

                                if pt_idx < 511 && pt[pt_idx + 1].is_present() {
                                    state = Printer::EmitDots;
                                } else {
                                    state = Printer::EmitLine;
                                }
                            }
                        }
                        Printer::EmitDots => {
                            if node_label.len() > 0 {
                                node_label += " | "
                            }
                            node_label += "...";

                            if pt_idx < 511 && pt[pt_idx + 1].is_present() {
                                state = Printer::Skip;
                            } else {
                                state = Printer::EmitLine;
                            }
                        }
                        Printer::Skip => {
                            if pt_idx < 511 && pt[pt_idx + 1].is_present() {
                                state = Printer::Skip;
                            } else {
                                state = Printer::EmitLine;
                            }
                        }
                    }
                }
                node_label
            }
            Nd::PD(pd, _) => {
                let mut state = Printer::EmitLine;
                for pd_idx in 0..pd.len() {
                    if pd_idx == 511 {
                        state = Printer::EmitLine;
                    }

                    let pd_item = pd[pd_idx];

                    match state {
                        Printer::EmitLine => {
                            if pd_item.is_present() {
                                if node_label.len() > 0 {
                                    node_label += " | "
                                }
                                node_label +=
                                    format!("<f{}> {:#x}", pd_idx, pd_item.address(),).as_str();

                                if pd_idx < 511 && pd[pd_idx + 1].is_present() {
                                    state = Printer::EmitDots;
                                } else {
                                    state = Printer::EmitLine;
                                }
                            }
                        }
                        Printer::EmitDots => {
                            if node_label.len() > 0 {
                                node_label += " | "
                            }
                            node_label += "...";

                            if pd_idx < 511 && pd[pd_idx + 1].is_present() {
                                state = Printer::Skip;
                            } else {
                                state = Printer::EmitLine;
                            }
                        }
                        Printer::Skip => {
                            if pd_idx < 511 && pd[pd_idx + 1].is_present() {
                                state = Printer::Skip;
                            } else {
                                state = Printer::EmitLine;
                            }
                        }
                    }
                }
                node_label
            }
            Nd::PDPT(pdpt, _) => {
                for (pdpt_idx, pdpt_item) in pdpt.iter().enumerate() {
                    if pdpt_item.is_present() {
                        if node_label.len() > 0 {
                            node_label += " | "
                        }
                        node_label +=
                            format!("<f{}> {:#x}", pdpt_idx, pdpt_item.address(),).as_str();
                    }
                }
                node_label
            }
            Nd::PML4(pml4, _) => {
                for (pml_idx, pml_item) in pml4.iter().enumerate() {
                    if pml_item.is_present() {
                        if node_label.len() > 0 {
                            node_label += " | "
                        }
                        node_label += format!("<f{}> {:#x}", pml_idx, pml_item.address(),).as_str();
                    }
                }
                node_label
            }
            Nd::Page(addr) => format!("Page4K_{:#x}", addr),
            Nd::LargePage(addr) => format!("Page2MiB_{:#x}", addr),
            Nd::HugePage(addr) => format!("Page1GiB_{:#x}", addr),
        };

        dot::LabelText::label(label)
    }

    fn node_id(&'a self, n: &Nd) -> dot::Id<'a> {
        let label = match n {
            Nd::PT(pt, None) => format!("PT_{:p}", *pt),
            Nd::PD(pd, None) => format!("PD_{:p}", *pd),
            Nd::PDPT(pdpt, None) => format!("PDPT_{:p}", *pdpt),
            Nd::PML4(pml4, None) => format!("PDPT_{:p}", *pml4),
            Nd::PT(pt, Some(slot)) => format!("PT_{:p}:f{}", *pt, slot),
            Nd::PD(pd, Some(slot)) => format!("PD_{:p}:f{}", *pd, slot),
            Nd::PDPT(pdpt, Some(slot)) => format!("PDPT_{:p}:f{}", *pdpt, slot),
            Nd::PML4(pml4, Some(slot)) => format!("PML4_{:p}:f{}", *pml4, slot),
            Nd::Page(addr) => format!("Page4K_{:#x}", addr),
            Nd::LargePage(addr) => format!("Page2MiB_{:#x}", addr),
            Nd::HugePage(addr) => format!("Page1GiB_{:#x}", addr),
        };

        dot::Id::new(label).expect("Can't make label")
    }
}

impl VSpace {
    fn parse_nodes_edges<'a>(&'a self) -> (dot::Nodes<'a, Nd<'a>>, dot::Edges<'a, Ed<'a>>) {
        let mut nodes = Vec::with_capacity(128);
        let mut edges = Vec::with_capacity(128);

        let pml4_table = self.pml4.as_ref();
        nodes.push(Nd::PML4(pml4_table, None));

        unsafe {
            for (pml_idx, pml_item) in pml4_table.iter().enumerate() {
                let from = Nd::PML4(pml4_table, None);

                if pml_item.is_present() {
                    let pdpt_table =
                        transmute::<VAddr, &mut PDPT>(VAddr::from_u64(pml_item.address().as_u64()));
                    let to = Nd::PDPT(pdpt_table, None);
                    nodes.push(to.clone());
                    edges.push(((from.clone(), pml_idx), (to.clone(), 0)));

                    let from = to;
                    for (pdpt_idx, pdpt_item) in pdpt_table.iter().enumerate() {
                        if pdpt_item.is_present() {
                            let pd_table = transmute::<VAddr, &mut PD>(VAddr::from_u64(
                                pdpt_item.address().as_u64(),
                            ));
                            if pdpt_item.is_page() {
                                let vaddr: usize = (512 * (512 * (512 * 0x1000))) * pml_idx
                                    + (512 * (512 * 0x1000)) * pdpt_idx;
                                let to = Nd::HugePage(pdpt_item.address());
                            //nodes.push(to.clone());
                            //edges.push((from.clone(), to.clone()));
                            } else {
                                let to = Nd::PD(pd_table, None);
                                nodes.push(to.clone());
                                edges.push(((from.clone(), pdpt_idx), (to.clone(), 0)));

                                let from = to;
                                for (pd_idx, pd_item) in pd_table.iter().enumerate() {
                                    if pd_item.is_present() {
                                        let ptes = transmute::<VAddr, &mut PT>(VAddr::from_u64(
                                            pd_item.address().as_u64(),
                                        ));

                                        if pd_item.is_page() {
                                            let vaddr: usize = (512 * (512 * (512 * 0x1000)))
                                                * pml_idx
                                                + (512 * (512 * 0x1000)) * pdpt_idx
                                                + (512 * 0x1000) * pd_idx;
                                            let to = Nd::LargePage(pd_item.address());
                                        //nodes.push(to.clone());
                                        //edges.push((from.clone(), to.clone()));
                                        } else {
                                            let to = Nd::PT(ptes, None);
                                            nodes.push(to.clone());
                                            edges.push(((from.clone(), pd_idx), (to.clone(), 0)));

                                            let from = to.clone();
                                            assert!(!pd_item.is_page());
                                            for (pte_idx, pte) in ptes.iter().enumerate() {
                                                let vaddr: usize = (512 * (512 * (512 * 0x1000)))
                                                    * pml_idx
                                                    + (512 * (512 * 0x1000)) * pdpt_idx
                                                    + (512 * 0x1000) * pd_idx
                                                    + (0x1000) * pte_idx;

                                                if pte.is_present() {
                                                    //let to = Nd::Page(pte.address());
                                                    //nodes.push(to.clone());
                                                    //edges.push((from.clone(), to.clone()));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        (nodes.into(), edges.into())
    }
}

impl<'a> dot::GraphWalk<'a> for VSpace {
    type Node = Nd<'a>;
    type Edge = Ed<'a>;
    fn nodes(&self) -> dot::Nodes<'a, Nd> {
        let (nodes, _) = self.parse_nodes_edges();
        nodes.into()
    }

    fn edges(&'a self) -> dot::Edges<'a, Ed> {
        let (_, edges) = self.parse_nodes_edges();
        edges.into()
    }

    fn source(&self, e: &Ed<'a>) -> Nd<'a> {
        match (e.0).0 {
            Nd::HugePage(_) => (e.0).0,
            Nd::LargePage(_) => (e.0).0,
            Nd::Page(_) => (e.0).0,
            Nd::PT(ptr, None) => Nd::PT(ptr, Some((e.0).1)),
            Nd::PD(ptr, None) => Nd::PD(ptr, Some((e.0).1)),
            Nd::PDPT(ptr, None) => Nd::PDPT(ptr, Some((e.0).1)),
            Nd::PML4(ptr, None) => Nd::PML4(ptr, Some((e.0).1)),
            _ => unimplemented!(),
        }
    }

    fn target(&self, e: &Ed<'a>) -> Nd<'a> {
        match (e.1).0 {
            Nd::HugePage(_) => (e.1).0,
            Nd::LargePage(_) => (e.1).0,
            Nd::Page(_) => (e.1).0,
            Nd::PT(ptr, None) => Nd::PT(ptr, Some((e.1).1)),
            Nd::PD(ptr, None) => Nd::PD(ptr, Some((e.1).1)),
            Nd::PDPT(ptr, None) => Nd::PDPT(ptr, Some((e.1).1)),
            Nd::PML4(ptr, None) => Nd::PML4(ptr, Some((e.1).1)),
            _ => unimplemented!(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::*;
    use core::ptr;
    use proptest::prelude::*;

    prop_compose! {
        fn base_pages(max: u64)(base in 0..max) -> u64 { base & !0xfff }
    }

    prop_compose! {
        fn large_pages(max: u64)(base in 0..max) -> u64 { base & !0x1fffff }
    }

    fn map_strategy() -> impl Strategy<Value = MapAction> {
        prop_oneof![
            //Just(MapAction::None),
            Just(MapAction::ReadUser),
            Just(MapAction::ReadKernel),
            Just(MapAction::ReadWriteUser),
            Just(MapAction::ReadWriteKernel),
            Just(MapAction::ReadExecuteUser),
            Just(MapAction::ReadExecuteKernel),
            Just(MapAction::ReadWriteExecuteUser),
            Just(MapAction::ReadWriteExecuteKernel),
        ]
    }

    proptest! {
        #[test]
        fn map_base_doesnt_crash(base in base_pages(0xffee_0000), size in base_pages(0xff_0000), action in map_strategy()) {
            crate::arch::start(0, ptr::null_mut());
            let mut vspace = VSpace::new();

            let base = VAddr::from(base);
            let size = size as usize;
            let rights = action;
            let palignment = BASE_PAGE_SIZE as u64;

            vspace
                .map(base, size, rights, palignment)
                .expect("Can't map stuff");
        }

        #[test]
        fn map_large_doesnt_crash(base in large_pages(0xffee_0000), size in base_pages(0xff_0000), action in map_strategy()) {
            crate::arch::start(0, ptr::null_mut());

            let mut vspace = VSpace::new();

            let base = VAddr::from(base);
            let size = size as usize;
            let rights = action;
            let palignment = BASE_PAGE_SIZE as u64;

            vspace
                .map(base, size, rights, palignment)
                .expect("Can't map stuff");
        }
    }
}
