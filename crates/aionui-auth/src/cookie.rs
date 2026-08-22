use aionui_common::constants::{COOKIE_MAX_AGE_DAYS, COOKIE_NAME, CSRF_COOKIE_NAME, REFRESH_COOKIE_NAME};

/// Cookie security configuration derived from the deployment environment.
#[derive(Debug, Clone)]
pub struct CookieConfig {
    /// Whether to set the `Secure` flag on cookies (HTTPS only).
    pub secure: bool,
    /// `SameSite` policy: `"Strict"` for HTTPS, `"Lax"` for HTTP.
    pub same_site: &'static str,
}

impl CookieConfig {
    /// Create cookie config from environment variables.
    ///
    /// - `AIONUI_HTTPS=true` → Secure flag, SameSite=Strict
    /// - Otherwise → no Secure flag, SameSite=Lax (for remote HTTP access)
    pub fn from_env() -> Self {
        let https = std::env::var("AIONUI_HTTPS")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            secure: https,
            same_site: if https { "Strict" } else { "Lax" },
        }
    }

    /// Build `Set-Cookie` header value for the session token.
    ///
    /// Attributes: HttpOnly, SameSite, Secure (if HTTPS), Max-Age=30d.
    pub fn build_session_cookie(&self, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite={}{}; Max-Age={max_age}",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value that clears the session cookie.
    pub fn clear_session_cookie(&self) -> String {
        format!(
            "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite={}{}; Max-Age=0",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    pub fn build_external_access_cookie(&self, token: &str, max_age_secs: u64) -> String {
        self.build_http_only_cookie(COOKIE_NAME, token, "/", max_age_secs)
    }

    pub fn build_external_refresh_cookie(&self, credential: &str, max_age_secs: u64) -> String {
        self.build_http_only_cookie(
            REFRESH_COOKIE_NAME,
            credential,
            "/api/auth/internal/external-sessions",
            max_age_secs,
        )
    }

    pub fn clear_external_refresh_cookie(&self) -> String {
        self.build_http_only_cookie(REFRESH_COOKIE_NAME, "", "/api/auth/internal/external-sessions", 0)
    }

    fn build_http_only_cookie(&self, name: &str, value: &str, path: &str, max_age_secs: u64) -> String {
        format!(
            "{name}={value}; Path={path}; HttpOnly; SameSite={}{}; Max-Age={max_age_secs}",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value for the CSRF token.
    ///
    /// NOT HttpOnly — JavaScript must read this value to include it
    /// in the `x-csrf-token` request header (Double Submit Cookie pattern).
    pub fn build_csrf_cookie(&self, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{CSRF_COOKIE_NAME}={token}; Path=/; SameSite={}{}; Max-Age={max_age}",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_config() -> CookieConfig {
        CookieConfig {
            secure: false,
            same_site: "Lax",
        }
    }

    fn https_config() -> CookieConfig {
        CookieConfig {
            secure: true,
            same_site: "Strict",
        }
    }

    #[test]
    fn session_cookie_http() {
        let cookie = http_config().build_session_cookie("my_token");
        assert!(cookie.contains("aionui-session=my_token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age="));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_https() {
        let cookie = https_config().build_session_cookie("my_token");
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn clear_session_cookie_sets_max_age_zero() {
        let cookie = http_config().clear_session_cookie();
        assert!(cookie.contains("aionui-session="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }

    #[test]
    fn csrf_cookie_not_http_only() {
        let cookie = http_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("aionui-csrf-token=csrf_abc"));
        assert!(!cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age="));
    }

    #[test]
    fn csrf_cookie_https_has_secure() {
        let cookie = https_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn session_cookie_max_age_30_days() {
        let cookie = http_config().build_session_cookie("t");
        let expected = 30 * 24 * 60 * 60;
        assert!(cookie.contains(&format!("Max-Age={expected}")));
    }

    #[test]
    fn renewable_external_cookies_have_distinct_scopes_and_lifetimes() {
        let config = http_config();
        let access = config.build_external_access_cookie("access", 900);
        let refresh = config.build_external_refresh_cookie("sid.secret", 2_592_000);

        assert_eq!(
            access,
            "aionui-session=access; Path=/; HttpOnly; SameSite=Lax; Max-Age=900"
        );
        assert_eq!(
            refresh,
            "aionui-refresh-session=sid.secret; Path=/api/auth/internal/external-sessions; HttpOnly; SameSite=Lax; Max-Age=2592000"
        );
    }

    #[test]
    fn matching_revoke_clears_refresh_cookie_on_its_original_path() {
        assert_eq!(
            http_config().clear_external_refresh_cookie(),
            "aionui-refresh-session=; Path=/api/auth/internal/external-sessions; HttpOnly; SameSite=Lax; Max-Age=0"
        );
    }
}
