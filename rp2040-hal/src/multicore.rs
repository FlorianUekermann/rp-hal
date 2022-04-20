//! Multicore support
//!
//! This module handles setup of the 2nd cpu core on the rp2040, which we refer to as core1.
//! It provides functionality for setting up the stack, and starting core1.
//!
//! The options for an entrypoint for core1 are
//! - a function that never returns - eg
//! `fn core1_task() -> ! { loop{} }; `
//! - a lambda (note: This requires a global allocator which requires a nightly compiler. Not recommended for beginners)
//!
//! # Usage
//!
//! ```no_run
//! static mut CORE1_STACK: Stack<4096> = Stack::new();
//! fn core1_task() -> ! {
//!     loop{}
//! }
//! // fn main() -> ! {
//!     use rp2040_hal::{pac, gpio::Pins, sio::Sio, multicore::{Multicore, Stack}};
//!     let mut pac = pac::Peripherals::take().unwrap();
//!     let mut sio = Sio::new(pac.SIO);
//!     // Other init code above this line
//!     let mut mc = Multicore::new(&mut pac.PSM, &mut pac.PPB, &mut sio);
//!     let cores = mc.cores();
//!     let core1 = &mut cores[1];
//!     let _test = core1.spawn(core1_task, unsafe { &mut CORE1_STACK.mem });
//!     // The rest of your application below this line
//! //}
//!
//! ```
//!
//! For inter-processor communications, see [`crate::sio::SioFifo`] and [`crate::sio::Spinlock0`]
//!
//! For a detailed example, see [examples/multicore_fifo_blink.rs](https://github.com/rp-rs/rp-hal/tree/main/rp2040-hal/examples/multicore_fifo_blink.rs)

use core::mem::ManuallyDrop;

use crate::pac;
use crate::Sio;

/// Errors for multicore operations.
#[derive(Debug)]
pub enum Error {
    /// Operation is invalid on this core.
    InvalidCore,
    /// Core was unresposive to commands.
    Unresponsive,
}

#[inline(always)]
fn install_stack_guard(stack_bottom: *mut usize) {
    let core = unsafe { pac::CorePeripherals::steal() };

    // Trap if MPU is already configured
    if core.MPU.ctrl.read() != 0 {
        cortex_m::asm::udf();
    }

    // The minimum we can protect is 32 bytes on a 32 byte boundary, so round up which will
    // just shorten the valid stack range a tad.
    let addr = (stack_bottom as u32 + 31) & !31;
    // Mask is 1 bit per 32 bytes of the 256 byte range... clear the bit for the segment we want
    let subregion_select = 0xff ^ (1 << ((addr >> 5) & 7));
    unsafe {
        core.MPU.ctrl.write(5); // enable mpu with background default map
        core.MPU.rbar.write((addr & !0xff) | 0x8);
        core.MPU.rasr.write(
            1 // enable region
               | (0x7 << 1) // size 2^(7 + 1) = 256
               | (subregion_select << 8)
               | 0x10000000, // XN = disable instruction fetch; no other bits means no permissions
        );
    }
}

#[inline(always)]
fn core1_setup(stack_bottom: *mut usize) {
    install_stack_guard(stack_bottom);
    // TODO: irq priorities
}

/// Multicore execution management.
pub struct Multicore<'p> {
    cores: [Core<'p>; 2],
}

/// Data type for a properly aligned stack of N 32-bit (usize) words
#[repr(C, align(32))]
pub struct Stack<const SIZE: usize> {
    /// Memory to be used for the stack
    pub mem: [usize; SIZE],
}

impl<const SIZE: usize> Stack<SIZE> {
    /// Construct a stack of length SIZE, initialized to 0
    pub const fn new() -> Stack<SIZE> {
        Stack { mem: [0; SIZE] }
    }
}

impl<'p> Multicore<'p> {
    /// Create a new |Multicore| instance.
    pub fn new(psm: &'p mut pac::PSM, ppb: &'p mut pac::PPB, sio: &'p mut crate::Sio) -> Self {
        Self {
            cores: [
                Core { inner: None },
                Core {
                    inner: Some((psm, ppb, sio)),
                },
            ],
        }
    }

    /// Get the available |Core| instances.
    pub fn cores(&mut self) -> &'p mut [Core] {
        &mut self.cores
    }
}

/// A handle for controlling a logical core.
pub struct Core<'p> {
    inner: Option<(&'p mut pac::PSM, &'p mut pac::PPB, &'p mut crate::Sio)>,
}

impl<'p> Core<'p> {
    /// Get the id of this core.
    pub fn id(&self) -> u8 {
        match self.inner {
            None => 0,
            Some(..) => 1,
        }
    }

    /// Spawn a function on this core.
    pub fn spawn<F>(&mut self, entry: F, stack: &'static mut [usize]) -> Result<(), Error>
    where
        F: FnOnce() -> bad::Never + Send + 'static,
    {
        if let Some((psm, ppb, sio)) = self.inner.as_mut() {
            // The first two ignored `u64` parameters are there to take up all of the registers,
            // which means that the rest of the arguments are taken from the stack,
            // where we're able to put them from core 0.
            extern "C" fn core1_startup<F: FnOnce() -> bad::Never>(
                _: u64,
                _: u64,
                entry: &mut ManuallyDrop<F>,
                stack_bottom: *mut usize,
            ) -> ! {
                core1_setup(stack_bottom);

                let entry = unsafe { ManuallyDrop::take(entry) };

                // Signal that it's safe for core 0 to get rid of the original value now.
                //
                // We don't have any way to get at core 1's SIO without using `Peripherals::steal` right now,
                // since svd2rust doesn't really support multiple cores properly.
                let peripherals = unsafe { pac::Peripherals::steal() };
                let mut sio = Sio::new(peripherals.SIO);
                sio.fifo.write_blocking(1);

                entry()
            }

            // Reset the core
            psm.frce_off.modify(|_, w| w.proc1().set_bit());
            while !psm.frce_off.read().proc1().bit_is_set() {
                cortex_m::asm::nop();
            }
            psm.frce_off.modify(|_, w| w.proc1().clear_bit());

            // Set up the stack
            let mut stack_ptr = unsafe { stack.as_mut_ptr().add(stack.len()) };

            let mut push = |v: usize| unsafe {
                stack_ptr = stack_ptr.sub(1);
                stack_ptr.write(v);
            };

            // We don't want to drop this, since it's getting moved to the other core.
            let mut entry = ManuallyDrop::new(entry);

            // Push the arguments to `core1_startup` onto the stack.
            push(stack.as_mut_ptr() as usize);
            push(&mut entry as *mut _ as usize);

            let vector_table = ppb.vtor.read().bits();

            // After reset, core 1 is waiting to receive commands over FIFO.
            // This is the sequence to have it jump to some code.
            let cmd_seq = [
                0,
                0,
                1,
                vector_table as usize,
                stack_ptr as usize,
                core1_startup::<F> as usize,
            ];

            let mut seq = 0;
            let mut fails = 0;
            loop {
                let cmd = cmd_seq[seq] as u32;
                if cmd == 0 {
                    sio.fifo.drain();
                    cortex_m::asm::sev();
                }
                sio.fifo.write_blocking(cmd);
                let response = sio.fifo.read_blocking();
                if cmd == response {
                    seq += 1;
                } else {
                    seq = 0;
                    fails += 1;
                    if fails > 16 {
                        return Err(Error::Unresponsive);
                    }
                }
                if seq >= cmd_seq.len() {
                    break;
                }
            }

            // Wait until the other core has copied `entry` before returning.
            sio.fifo.read_blocking();

            Ok(())
        } else {
            Err(Error::InvalidCore)
        }
    }
}

// https://github.com/nvzqz/bad-rs/blob/master/src/never.rs
mod bad {
    pub(crate) type Never = <F as HasOutput>::Output;

    pub trait HasOutput {
        type Output;
    }

    impl<O> HasOutput for fn() -> O {
        type Output = O;
    }

    type F = fn() -> !;
}
