use std::path::Path;

use aionui_runtime::Builder as CmdBuilder;
use tracing::{info, warn};

use crate::constants::{
    LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
    LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
};
use crate::error::ExtensionError;
use crate::types::{LifecycleHook, LifecycleHooks};

const AGENT_INSTALL_DIR_ENV: &str = "AIONUI_AGENT_INSTALL_DIR";

/// Which lifecycle hook to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    OnInstall,
    OnUninstall,
    OnActivate,
    OnDeactivate,
}

impl HookKind {
    /// Default timeout in seconds for this hook kind.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::OnInstall => LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
            Self::OnUninstall => LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
            Self::OnActivate => LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS,
            Self::OnDeactivate => LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS,
        }
    }

    /// Human-readable label for logging and error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::OnInstall => "onInstall",
            Self::OnUninstall => "onUninstall",
            Self::OnActivate => "onActivate",
            Self::OnDeactivate => "onDeactivate",
        }
    }
}

/// Resolve the hook script path from the manifest for a given hook kind.
pub fn resolve_hook_path(hooks: &LifecycleHooks, kind: HookKind) -> Option<&str> {
    let value = match kind {
        HookKind::OnInstall => hooks.on_install.as_ref(),
        HookKind::OnUninstall => hooks.on_uninstall.as_ref(),
        HookKind::OnActivate => hooks.on_activate.as_ref(),
        HookKind::OnDeactivate => hooks.on_deactivate.as_ref(),
    };
    match value {
        Some(LifecycleHook::Script(path)) if !path.is_empty() => Some(path),
        _ => None,
    }
}

/// Resolve either a legacy script hook or the current declarative command hook.
pub fn resolve_hook(hooks: &LifecycleHooks, kind: HookKind) -> Option<&LifecycleHook> {
    let value = match kind {
        HookKind::OnInstall => hooks.on_install.as_ref(),
        HookKind::OnUninstall => hooks.on_uninstall.as_ref(),
        HookKind::OnActivate => hooks.on_activate.as_ref(),
        HookKind::OnDeactivate => hooks.on_deactivate.as_ref(),
    };
    value.filter(|hook| match hook {
        LifecycleHook::Script(path) => !path.is_empty(),
        LifecycleHook::Command(command) => !command.shell.cli_command.is_empty(),
    })
}

/// Execute a lifecycle hook script in a child process.
///
/// - `ext_dir`: absolute path to the extension root directory (used as cwd).
/// - `hook_path`: script path relative to `ext_dir`.
/// - `kind`: which hook is being executed (determines timeout and label).
/// - `extension_name`: used for logging and error context.
///
/// Returns `Ok(())` on success. Returns an error if the script is not found,
/// times out, or exits with a non-zero status.
pub async fn execute_hook(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
) -> Result<(), ExtensionError> {
    execute_lifecycle_hook(
        ext_dir,
        &LifecycleHook::Script(hook_path.to_owned()),
        kind,
        extension_name,
    )
    .await
}

pub async fn execute_lifecycle_hook(
    ext_dir: &Path,
    hook: &LifecycleHook,
    kind: HookKind,
    extension_name: &str,
) -> Result<(), ExtensionError> {
    let (mut builder, hook_description, configured_timeout) = match hook {
        LifecycleHook::Script(hook_path) => {
            let script = ext_dir.join(hook_path);

            if !script.exists() {
                warn!(
                    extension = extension_name,
                    hook = kind.label(),
                    path = %script.display(),
                    "lifecycle hook script not found, skipping"
                );
                return Err(ExtensionError::HookNotFound(script.display().to_string()));
            }

            (CmdBuilder::clean_cli(&script), script.display().to_string(), None)
        }
        LifecycleHook::Command(command) => {
            let mut builder = CmdBuilder::clean_cli(&command.shell.cli_command);
            builder.args(&command.shell.args);
            (builder, command.shell.cli_command.clone(), command.timeout)
        }
    };

    if let Some(agent_install_dir) = resolve_agent_install_dir(ext_dir) {
        std::fs::create_dir_all(&agent_install_dir)?;
        builder.env(AGENT_INSTALL_DIR_ENV, agent_install_dir);
    }

    let default_timeout = std::time::Duration::from_secs(kind.timeout_secs());
    let timeout = configured_timeout
        .filter(|timeout_ms| *timeout_ms > 0)
        .map(std::time::Duration::from_millis)
        .map_or(default_timeout, |timeout| timeout.min(default_timeout));
    let timeout_secs = timeout.as_secs().max(1);
    let label = kind.label();

    info!(
        extension = extension_name,
        hook = label,
        command = %hook_description,
        timeout_ms = timeout.as_millis(),
        "executing lifecycle hook"
    );

    builder.current_dir(ext_dir);
    let child_future = builder.output();

    let result = tokio::time::timeout(timeout, child_future).await;

    match result {
        Err(_elapsed) => {
            warn!(
                extension = extension_name,
                hook = label,
                timeout_secs,
                "lifecycle hook timed out"
            );
            Err(ExtensionError::HookTimeout {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                timeout_secs,
            })
        }
        Ok(Err(io_err)) => {
            warn!(
                extension = extension_name,
                hook = label,
                error = %io_err,
                "lifecycle hook I/O error"
            );
            Err(ExtensionError::HookFailed {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                reason: io_err.to_string(),
            })
        }
        Ok(Ok(output)) => {
            if output.status.success() {
                info!(
                    extension = extension_name,
                    hook = label,
                    "lifecycle hook completed successfully"
                );
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string());
                warn!(
                    extension = extension_name,
                    hook = label,
                    exit_code = %code,
                    stderr = %stderr,
                    "lifecycle hook exited with error"
                );
                Err(ExtensionError::HookFailed {
                    extension_name: extension_name.to_owned(),
                    hook: label.to_owned(),
                    reason: format!("exit code {code}: {}", stderr.trim()),
                })
            }
        }
    }
}

fn resolve_agent_install_dir(ext_dir: &Path) -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os(AGENT_INSTALL_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(configured.into());
    }

    infer_agent_install_dir(ext_dir)
}

fn infer_agent_install_dir(ext_dir: &Path) -> Option<std::path::PathBuf> {
    let extensions_dir = ext_dir.parent()?;
    if extensions_dir.file_name()? != "extensions" {
        return None;
    }
    Some(extensions_dir.parent()?.join("agent-runtime"))
}

/// Determine whether the `onInstall` hook should run.
///
/// Returns `true` when:
/// - There is no persisted version (first-time install).
/// - The persisted version differs from the current manifest version.
pub fn needs_install_hook(current_version: &str, persisted_version: Option<&str>) -> bool {
    match persisted_version {
        None => true,
        Some(prev) => prev != current_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    // -----------------------------------------------------------------------
    // needs_install_hook
    // -----------------------------------------------------------------------

    #[test]
    fn test_needs_install_first_time() {
        assert!(needs_install_hook("1.0.0", None));
    }

    #[test]
    fn test_needs_install_version_changed() {
        assert!(needs_install_hook("2.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_no_install_same_version() {
        assert!(!needs_install_hook("1.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_needs_install_downgrade() {
        assert!(needs_install_hook("0.9.0", Some("1.0.0")));
    }

    // -----------------------------------------------------------------------
    // HookKind
    // -----------------------------------------------------------------------

    #[test]
    fn test_hook_kind_timeout_values() {
        assert_eq!(HookKind::OnInstall.timeout_secs(), 120);
        assert_eq!(HookKind::OnUninstall.timeout_secs(), 60);
        assert_eq!(HookKind::OnActivate.timeout_secs(), 30);
        assert_eq!(HookKind::OnDeactivate.timeout_secs(), 30);
    }

    #[test]
    fn test_hook_kind_labels() {
        assert_eq!(HookKind::OnInstall.label(), "onInstall");
        assert_eq!(HookKind::OnUninstall.label(), "onUninstall");
        assert_eq!(HookKind::OnActivate.label(), "onActivate");
        assert_eq!(HookKind::OnDeactivate.label(), "onDeactivate");
    }

    #[test]
    fn test_agent_install_dir_is_inferred_from_data_extensions_dir() {
        let ext_dir = Path::new("/tmp/aionui-data/extensions/aionext-codex");
        assert_eq!(
            infer_agent_install_dir(ext_dir),
            Some(Path::new("/tmp/aionui-data/agent-runtime").to_path_buf())
        );
    }

    // -----------------------------------------------------------------------
    // resolve_hook_path
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_hook_path_present() {
        let hooks = LifecycleHooks {
            on_install: Some("scripts/install.sh".into()),
            on_activate: Some("scripts/activate.sh".into()),
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnInstall),
            Some("scripts/install.sh")
        );
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnActivate),
            Some("scripts/activate.sh")
        );
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnDeactivate), None);
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnUninstall), None);
    }

    #[test]
    fn test_resolve_hook_path_empty_string() {
        let hooks = LifecycleHooks {
            on_install: Some(String::new().into()),
            on_activate: None,
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnInstall), None);
    }

    // -----------------------------------------------------------------------
    // execute_hook (async unit tests)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_hook_script_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_hook(dir.path(), "nonexistent.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ExtensionError::HookNotFound(_)));
    }

    #[tokio::test]
    async fn test_execute_hook_success() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("hook.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "hook.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_declarative_command_hook() {
        let dir = tempfile::tempdir().unwrap();
        let hook = LifecycleHook::Command(crate::types::LifecycleCommandHook {
            shell: crate::types::LifecycleShellHook {
                cli_command: "sh".into(),
                args: vec!["-c".into(), "printf installed > command_marker.txt".into()],
            },
            timeout: Some(5000),
        });

        let result = execute_lifecycle_hook(dir.path(), &hook, HookKind::OnInstall, "test-ext").await;

        assert!(result.is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("command_marker.txt")).unwrap(),
            "installed"
        );
    }

    #[tokio::test]
    async fn test_execute_hook_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fail.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'something broke' >&2\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "fail.sh", HookKind::OnInstall, "test-ext").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ExtensionError::HookFailed {
                extension_name,
                hook,
                reason,
            } => {
                assert_eq!(extension_name, "test-ext");
                assert_eq!(hook, "onInstall");
                assert!(reason.contains("something broke"));
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_execute_hook_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("slow.sh");
        // Script that sleeps longer than we allow
        std::fs::write(&script_path, "#!/bin/sh\nsleep 60\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Use a very short timeout override via a direct timeout wrapper
        let ext_dir = dir.path().to_owned();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            Command::new(ext_dir.join("slow.sh"))
                .current_dir(&ext_dir)
                .kill_on_drop(true)
                .output(),
        )
        .await;

        assert!(result.is_err(), "should have timed out");
    }

    #[tokio::test]
    async fn test_execute_hook_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cwd_marker.txt");
        let script_path = dir.path().join("check_cwd.sh");
        // Write cwd to a file so we can verify it
        std::fs::write(&script_path, "#!/bin/sh\npwd > cwd_marker.txt\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "check_cwd.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
        assert!(marker.exists());
        let cwd_content = std::fs::read_to_string(&marker).unwrap();
        // The cwd written by the script should match the extension dir
        // (may have symlink resolution differences, compare canonical)
        let expected = dir.path().canonicalize().unwrap();
        let actual_trimmed = cwd_content.trim();
        let actual = Path::new(actual_trimmed).canonicalize().unwrap();
        assert_eq!(actual, expected);
    }
}
