// KCB is the local kernel control that stores all core local state.

use alloc::boxed::Box;
use core::cell::{Ref, RefCell, RefMut};
use core::pin::Pin;
use core::ptr;

use x86::current::segmentation;
use x86::msr::{wrmsr, IA32_KERNEL_GSBASE};

use apic::xapic::XAPIC;

use super::irq;
use super::process::Process;
use super::vspace::VSpace;

use crate::arch::{KernelArgs, Module};
use crate::memory::buddy::BuddyFrameAllocator;
use crate::memory::{PAddr, PhysicalMemoryAllocator};

/// Try to retrieve the KCB by reading the gs register.
pub fn try_get_kcb<'a>() -> Option<&'a mut Kcb> {
    unsafe {
        let kcb = segmentation::rdgsbase() as *mut Kcb;
        if kcb != ptr::null_mut() {
            let kptr = ptr::NonNull::new_unchecked(kcb);
            Some(&mut *kptr.as_ptr())
        } else {
            None
        }
    }
}

/// Retrieve the KCB by reading the gs register.
///
/// # Panic
/// This will fail in case the KCB is not yet set (i.e., early on during
/// initialization).
pub fn get_kcb<'a>() -> &'a mut Kcb {
    unsafe {
        let kcb = segmentation::rdgsbase() as *mut Kcb;
        assert!(kcb != ptr::null_mut(), "KCB not found in gs register.");
        let kptr = ptr::NonNull::new_unchecked(kcb);
        &mut *kptr.as_ptr()
    }
}

/// Installs the KCB by setting the gs register to point to it.
///
/// We also set IA32_KERNEL_GSBASE to the kcb pointer to make sure
/// when we call swapgs on a syscall entry, we restore the pointer
/// to the KCB (since user-space may change gs register for
/// TLS etc.).
unsafe fn set_kcb(kcb: ptr::NonNull<Kcb>) {
    // Set up the GS register to point to the KCB
    segmentation::wrgsbase(kcb.as_ptr() as u64);
    // Set up swapgs instruction to reset the gs register to the KCB on irq, trap or syscall
    wrmsr(IA32_KERNEL_GSBASE, kcb.as_ptr() as u64);
}

/// Initialize the KCB in the system.
///
/// Should be called during set-up. Afterwards we can use `get_kcb` safely.
pub(crate) fn init_kcb(kcb: &mut Kcb) {
    let kptr: ptr::NonNull<Kcb> = ptr::NonNull::from(kcb);
    unsafe { set_kcb(kptr) };
}

/// The Kernel Control Block for a given core. It contains all core-local state of the kernel.
pub struct Kcb {
    /// Pointer to the syscall stack (this is referenced in assembly early on in exec.S)
    /// and should therefore always be at offset 0 of the Kcb struct!
    syscall_stack_top: *mut u8,
    /// Pointer to the save area of `current_process`,
    /// this is referenced on trap/syscall entries to save the CPU state into it.
    /// `current_process` == None implies `current_process_save_area` == NULL
    current_process_save_area: *mut kpi::arch::SaveArea,
    /// A handle to the currently active (scheduled) process.
    current_process: Option<RefCell<Process>>,
    /// Arguments passed to the kernel by the bootloader.
    kernel_args: RefCell<&'static KernelArgs<[Module; 2]>>,
    /// A pointer to the memory location of the kernel ELF binary.
    kernel_binary: RefCell<&'static [u8]>,
    /// The initial VSpace as constructed by the bootloader.
    init_vspace: RefCell<VSpace>,
    /// A handle to the physical memory manager.
    pmanager: RefCell<BuddyFrameAllocator>,
    /// A handle to the core-local interrupt driver.
    apic: RefCell<XAPIC>,
    /// The interrupt stack (that is used by the CPU on interrupts/traps/faults)
    ///
    /// The CPU switches to this memory location automatically (see gdt.rs).
    /// This member should probably not be touched from normal code.
    interrupt_stack: Option<Pin<Box<[u8; 64 * 0x1000]>>>,
    /// A handle to the syscall stack memory location.
    ///
    /// We switch rsp/rbp to point in here in exec.S.
    /// This member should probably not be touched from normal code.
    syscall_stack: Option<Pin<Box<[u8; 64 * 0x1000]>>>,
}

impl Kcb {
    pub fn new(
        kernel_args: &'static KernelArgs<[Module; 2]>,
        kernel_binary: &'static [u8],
        init_vspace: VSpace,
        pmanager: BuddyFrameAllocator,
        apic: XAPIC,
    ) -> Kcb {
        Kcb {
            syscall_stack_top: ptr::null_mut(),
            current_process_save_area: ptr::null_mut(),
            current_process: None,
            kernel_args: RefCell::new(kernel_args),
            kernel_binary: RefCell::new(kernel_binary),
            init_vspace: RefCell::new(init_vspace),
            pmanager: RefCell::new(pmanager),
            apic: RefCell::new(apic),
            interrupt_stack: None,
            syscall_stack: None,
        }
    }

    pub fn set_syscall_stack(&mut self, mut stack: Pin<Box<[u8; 64 * 0x1000]>>) {
        unsafe {
            self.syscall_stack_top = stack.as_mut_ptr().offset((stack.len()) as isize);
        }
        info!("syscall_stack_top {:p}", self.syscall_stack_top);
        self.syscall_stack = Some(stack);

        // self.syscall_stack_top should be at offset 0 (for assembly)
        debug_assert_eq!(
            (&self.syscall_stack_top as *const _ as usize) - (self as *const _ as usize),
            0
        );

        // the current process entry should be at offset 8 (for assembly)
        debug_assert_eq!(
            (&self.current_process_save_area as *const _ as usize) - (self as *const _ as usize),
            8
        );
    }

    pub fn pmanager(&self) -> RefMut<BuddyFrameAllocator> {
        self.pmanager.borrow_mut()
    }

    pub fn apic(&self) -> RefMut<XAPIC> {
        self.apic.borrow_mut()
    }

    pub fn init_vspace(&self) -> RefMut<VSpace> {
        self.init_vspace.borrow_mut()
    }

    pub fn kernel_binary(&self) -> Ref<&'static [u8]> {
        self.kernel_binary.borrow()
    }

    pub fn kernel_args(&self) -> Ref<&'static KernelArgs<[Module; 2]>> {
        self.kernel_args.borrow()
    }
}
