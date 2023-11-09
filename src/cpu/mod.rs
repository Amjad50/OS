use self::{gdt::GlobalDescriptorTablePointer, idt::InterruptDescriptorTablePointer};

pub mod gdt;
pub mod idt;
pub mod interrupts;

const CPUID_FN_FEAT: u32 = 1;
const MAX_CPUS: usize = 8;

pub mod flags {
    pub const IF: u64 = 1 << 9;
}

static mut CPUS: [Cpu; MAX_CPUS] = [Cpu::empty(); MAX_CPUS];

#[derive(Debug, Clone, Copy)]
pub struct Cpu {
    // index of myself inside `CPUS`
    pub id: usize,
    apic_id: u8,
    old_interrupt_enable: bool,
    // number of times we have called `cli`
    n_cli: usize,
}

impl Cpu {
    const fn empty() -> Self {
        Self {
            id: 0,
            apic_id: 0,
            old_interrupt_enable: false,
            n_cli: 0,
        }
    }

    fn init(&mut self, id: usize, apic_id: u8) {
        self.id = id;
        self.apic_id = apic_id;
    }

    pub fn push_cli(&mut self) {
        if self.n_cli == 0 {
            let rflags = unsafe { rflags() };
            let old_interrupt_flag = rflags & flags::IF != 0;
            unsafe { clear_interrupts() };
            self.old_interrupt_enable = old_interrupt_flag;
        }
        self.n_cli += 1;
    }

    pub fn pop_cli(&mut self) {
        let rflags = unsafe { rflags() };
        if rflags & flags::IF != 0 {
            panic!("interrupt shouldn't be set");
        }
        if self.n_cli == 0 {
            panic!("pop_cli called without push_cli");
        }

        self.n_cli -= 1;
        if self.n_cli == 0 && self.old_interrupt_enable {
            unsafe { set_interrupts() };
        }
    }
}

pub fn cpu() -> &'static mut Cpu {
    // TODO: use thread local to get the current cpu
    unsafe { &mut CPUS[0] }
}

pub unsafe fn rflags() -> u64 {
    let rflags: u64;
    core::arch::asm!("pushfq; pop {0:r}", out(reg) rflags, options(nomem, nostack, preserves_flags));
    rflags
}

pub unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    core::arch::asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

pub unsafe fn clear_interrupts() {
    core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
}

#[allow(dead_code)]
pub unsafe fn set_interrupts() {
    core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
}

pub unsafe fn set_cr3(cr3: u64) {
    core::arch::asm!("mov cr3, rax", in("rax") cr3, options(nomem, nostack, preserves_flags));
}

/// SAFETY: the data pointed to by `ldtr` must be static and never change
unsafe fn lgdt(ldtr: &GlobalDescriptorTablePointer) {
    core::arch::asm!("lgdt [rax]", in("rax") ldtr, options(nomem, nostack, preserves_flags));
}

unsafe fn lidt(ldtr: &InterruptDescriptorTablePointer) {
    core::arch::asm!("lidt [rax]", in("rax") ldtr, options(nomem, nostack, preserves_flags));
}

unsafe fn ltr(tr: u16) {
    core::arch::asm!("ltr ax", in("ax") tr, options(nomem, nostack, preserves_flags));
}

unsafe fn set_cs(cs: u16) {
    core::arch::asm!(
        "push {:r}",
        // this is not 0x1f, it is `1-forward`,
        // which gives the offset of the nearest `1:` label
        "lea {tmp}, [rip + 1f]",
        "push {tmp}",
        "retfq",
        "1:",
        in(reg) cs as u64, tmp=lateout(reg) _, options(preserves_flags));
}

fn get_cs() -> u16 {
    let cs: u16;
    unsafe {
        core::arch::asm!("mov {0:r}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
    }
    cs
}

pub unsafe fn rdmsr(inp: u32) -> u64 {
    let (eax, edx): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") inp, out("eax") eax, out("edx") edx, options(nomem, nostack, preserves_flags));
    ((edx as u64) << 32) | (eax as u64)
}

pub unsafe fn wrmsr(inp: u32, val: u64) {
    let eax = val as u32;
    let edx = (val >> 32) as u32;
    core::arch::asm!("wrmsr", in("ecx") inp, in("eax") eax, in("edx") edx, options(nomem, nostack, preserves_flags));
}

#[macro_export]
macro_rules! cpuid {
    ($rax:expr) => {
        ::core::arch::x86_64::__cpuid_count($rax, 0)
    };
    ($rax:expr, $rcx:expr) => {
        ::core::arch::x86_64::__cpuid_count($rax, $rcx)
    };
}
#[allow(unused_imports)]
pub use cpuid;