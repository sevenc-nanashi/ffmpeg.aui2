use clap::ValueEnum;
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, SetPriorityClass,
    SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL,
    THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_IDLE, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProcessPriority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl ProcessPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::BelowNormal => "below-normal",
            Self::Normal => "normal",
            Self::AboveNormal => "above-normal",
            Self::High => "high",
        }
    }

    fn as_windows_class(self) -> u32 {
        match self {
            Self::Idle => IDLE_PRIORITY_CLASS,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Self::Normal => NORMAL_PRIORITY_CLASS,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Self::High => HIGH_PRIORITY_CLASS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThreadPriority {
    Idle,
    Lowest,
    BelowNormal,
    Normal,
    AboveNormal,
    Highest,
}

impl ThreadPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Lowest => "lowest",
            Self::BelowNormal => "below-normal",
            Self::Normal => "normal",
            Self::AboveNormal => "above-normal",
            Self::Highest => "highest",
        }
    }

    fn as_windows_priority(self) -> i32 {
        match self {
            Self::Idle => THREAD_PRIORITY_IDLE,
            Self::Lowest => THREAD_PRIORITY_LOWEST,
            Self::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
            Self::Normal => THREAD_PRIORITY_NORMAL,
            Self::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
            Self::Highest => THREAD_PRIORITY_HIGHEST,
        }
    }
}

pub fn set_process_priority(priority: ProcessPriority) -> anyhow::Result<()> {
    let succeeded = unsafe { SetPriorityClass(GetCurrentProcess(), priority.as_windows_class()) };
    anyhow::ensure!(
        succeeded != 0,
        "SetPriorityClass failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub fn set_current_thread_priority(priority: ThreadPriority) -> anyhow::Result<()> {
    let succeeded =
        unsafe { SetThreadPriority(GetCurrentThread(), priority.as_windows_priority()) };
    anyhow::ensure!(
        succeeded != 0,
        "SetThreadPriority failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
