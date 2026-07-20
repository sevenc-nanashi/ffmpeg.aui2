use anyhow::Context;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

#[derive(Debug, Clone, Copy)]
pub struct ProcessMemory {
    pub working_set_bytes: u64,
    pub private_bytes: u64,
}

pub fn current_process_memory() -> anyhow::Result<ProcessMemory> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>())
            .expect("PROCESS_MEMORY_COUNTERS_EX size exceeds u32"),
        ..Default::default()
    };
    let succeeded = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    anyhow::ensure!(
        succeeded != 0,
        "K32GetProcessMemoryInfo failed: {}",
        std::io::Error::last_os_error()
    );

    Ok(ProcessMemory {
        working_set_bytes: u64::try_from(counters.WorkingSetSize)
            .context("WorkingSetSize exceeds u64")?,
        private_bytes: u64::try_from(counters.PrivateUsage).context("PrivateUsage exceeds u64")?,
    })
}
