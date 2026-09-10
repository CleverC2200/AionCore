use std::ffi::OsStr;
use std::io;
use std::process::{Command, Stdio};

use crate::scrub_core_only_env;

/// Open a target synchronously with the platform's default application.
pub fn open_path(target: impl AsRef<OsStr>) -> io::Result<()> {
    run_commands(open::commands(target))
}

/// Open a target through a detached launcher that may outlive AionCore.
pub fn open_path_detached(target: impl AsRef<OsStr>) -> io::Result<()> {
    spawn_commands_detached(open::commands(target))
}

fn run_commands(commands: Vec<Command>) -> io::Result<()> {
    let mut last_error = None;
    for mut command in commands {
        scrub_core_only_env(&mut command);
        match command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => return Err(io::Error::other(format!("Launcher {command:?} failed with {status:?}"))),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("open::commands always returns at least one launcher"))
}

fn spawn_commands_detached(commands: Vec<Command>) -> io::Result<()> {
    let mut last_error = None;
    for mut command in commands {
        scrub_core_only_env(&mut command);
        match spawn_detached(&mut command) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("open::commands always returns at least one launcher"))
}

fn spawn_detached(command: &mut Command) -> io::Result<()> {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt as _;

        // Match open::that_detached: fork once more before exec and create a
        // new session so the launcher can outlive AionCore.
        command.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn synchronous_and_detached_openers_scrub_explicit_core_credentials() {
        for detached in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let marker = temp.path().join("opener-env");
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(
                    "if [ -z \"${AIONCORE_BOOTSTRAP_SECRET+x}${AIONUI_GEA_SALES_PLAN_CLIENT_ID+x}${AIONUI_GEA_SALES_PLAN_CLIENT_SECRET+x}\" ]; then printf clean > \"$1\"; else printf leaked > \"$1\"; fi",
                )
                .arg("scrubbed-opener")
                .arg(&marker)
                .env("AIONCORE_BOOTSTRAP_SECRET", "forwarded-bootstrap")
                .env("AIONUI_GEA_SALES_PLAN_CLIENT_ID", "forwarded-client")
                .env("AIONUI_GEA_SALES_PLAN_CLIENT_SECRET", "forwarded-secret");

            if detached {
                spawn_commands_detached(vec![command]).unwrap();
            } else {
                run_commands(vec![command]).unwrap();
            }

            let deadline = Instant::now() + Duration::from_secs(2);
            while !marker.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "clean");
        }
    }
}
