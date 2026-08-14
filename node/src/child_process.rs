//! Child-process constructors for backend utility commands.
//!
//! `allmystuff-serve` is a console-subsystem binary, but the desktop app starts
//! it without a console. On Windows, launching another console-subsystem tool
//! (`net.exe`, `reg.exe`, PowerShell, `shutdown.exe`, or a replacement node)
//! without `CREATE_NO_WINDOW` can therefore flash a terminal on the desktop.
//! Keep the flag in one place so new backend utility calls do not regress it.

use std::ffi::OsStr;

/// A Tokio child command that never creates a visible Windows console.
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let command = tokio::process::Command::new(program);
    #[cfg(windows)]
    let mut command = command;
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
}

/// A blocking child command that never creates a visible Windows console.
pub fn blocking_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let command = std::process::Command::new(program);
    #[cfg(windows)]
    let mut command = command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}
