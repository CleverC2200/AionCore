use std::process::Command;

/// Credentials owned by the long-lived AionCore process.
///
/// These values must never be inherited or explicitly forwarded to helper,
/// runtime, agent, MCP, or desktop-opener subprocesses.
pub const CORE_ONLY_ENV_KEYS: &[&str] = &[
    "AIONCORE_BOOTSTRAP_SECRET",
    "AIONUI_GEA_SALES_PLAN_CLIENT_ID",
    "AIONUI_GEA_SALES_PLAN_CLIENT_SECRET",
];

/// Remove every Core-only credential from a standard-library command.
pub fn scrub_core_only_env(command: &mut Command) {
    for key in CORE_ONLY_ENV_KEYS {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scrubbed_commands_cannot_see_inherited_or_explicit_core_credentials() {
        if !crate::test_support::run_in_env_child(
            "core_env::tests::scrubbed_commands_cannot_see_inherited_or_explicit_core_credentials",
            |command| {
                command
                    .env("AIONCORE_BOOTSTRAP_SECRET", "bootstrap-secret")
                    .env("AIONUI_GEA_SALES_PLAN_CLIENT_ID", "sales-plan-client")
                    .env("AIONUI_GEA_SALES_PLAN_CLIENT_SECRET", "sales-plan-secret");
            },
        ) {
            return;
        }

        for explicit_override in [false, true] {
            let mut command = Command::new("sh");
            command.arg("-c").arg(
                "printf '%s:%s:%s' \
                 \"${AIONCORE_BOOTSTRAP_SECRET:-unset}\" \
                 \"${AIONUI_GEA_SALES_PLAN_CLIENT_ID:-unset}\" \
                 \"${AIONUI_GEA_SALES_PLAN_CLIENT_SECRET:-unset}\"",
            );
            if explicit_override {
                command
                    .env("AIONCORE_BOOTSTRAP_SECRET", "forwarded-bootstrap")
                    .env("AIONUI_GEA_SALES_PLAN_CLIENT_ID", "forwarded-client")
                    .env("AIONUI_GEA_SALES_PLAN_CLIENT_SECRET", "forwarded-secret");
            }
            scrub_core_only_env(&mut command);
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout), "unset:unset:unset");
        }
    }
}
