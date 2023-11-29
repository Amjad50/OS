use core::{cell::UnsafeCell, fmt, sync::atomic::AtomicI64};

use crate::cpu;

use super::lock;

pub struct Mutex<T> {
    lock: lock::Lock,
    owner_cpu: AtomicI64,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> fmt::Debug for Mutex<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex")
            .field("owner_cpu", &self.owner_cpu)
            .field("data", unsafe { &self.data.get().as_ref().unwrap() })
            .finish()
    }
}

#[must_use]
pub struct MutexGuard<'a, T: 'a> {
    lock: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(data: T) -> Self {
        Self {
            lock: lock::Lock::new(),
            owner_cpu: AtomicI64::new(-1),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> MutexGuard<T> {
        let cpu = cpu::cpu();
        cpu.push_cli(); // disable interrupts to avoid deadlock
        let cpu_id = cpu.id as i64;

        if self.owner_cpu.load(core::sync::atomic::Ordering::Relaxed) == cpu_id {
            panic!("Mutex already locked by this CPU");
        } else {
            // SAFETY: the mutex is locked, we are the only accessor
            unsafe { self.lock.lock() };
            self.owner_cpu
                .store(cpu_id, core::sync::atomic::Ordering::Relaxed);
            MutexGuard { lock: self }
        }
    }

    /// A special method to allow accessing the variable inside
    /// the lock after locking it.
    ///
    /// The difference between this and using `Deref` is that
    /// the lifetime of the returned reference is tied to main value of the lock.
    #[allow(dead_code)]
    pub fn run_with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        let guard: MutexGuard<'a, T> = self.lock();
        let d = unsafe { guard.lock.data.get().as_ref().unwrap() };
        f(d)
    }

    /// A special method to allow accessing the variable inside
    /// the lock after locking it.
    ///
    /// The difference between this and using `DerefMut` is that
    /// the lifetime of the returned reference is tied to main value of the lock.
    #[allow(dead_code)]
    pub fn run_with_mut<'a, R>(&'a self, f: impl FnOnce(&'a mut T) -> R) -> R {
        let guard: MutexGuard<'a, T> = self.lock();
        let d = unsafe { guard.lock.data.get().as_mut().unwrap() };
        f(d)
    }
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the mutex is locked, we are the only accessors,
        //         and the pointer is valid, since it was generated for a valid T
        unsafe { self.lock.data.get().as_ref().unwrap() }
    }
}

impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the mutex is locked, we are the only accessors,
        //         and the pointer is valid, since it was generated for a valid T
        unsafe { self.lock.data.get().as_mut().unwrap() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock
            .owner_cpu
            .store(-1, core::sync::atomic::Ordering::Relaxed);
        // SAFETY: the mutex is locked, we are the only accessor
        unsafe { self.lock.lock.unlock() };
        cpu::cpu().pop_cli(); // re-enable interrupts
    }
}