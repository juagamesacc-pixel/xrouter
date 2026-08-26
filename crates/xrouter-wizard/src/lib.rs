/// Self-contained HTML templates for the xrouter web wizard.
///
/// Each constant is a complete, standalone HTML page with inline CSS/JS.
/// Include via `include_str!` in the binary crate or serve directly.
///
/// # Usage
///
/// ```rust
/// // In your binary / server handler:
/// const WIZARD_HTML: &str = include_str!("templates/wizard.html");
/// const METRICS_HTML: &str = include_str!("templates/metrics.html");
///
/// // Serve as response body with Content-Type: text/html
/// ```

/// The main wizard page: provider selector, API key management with eye-toggle,
/// ban config, model fetcher, tier editor.
pub const WIZARD_HTML: &str = include_str!("templates/wizard.html");

/// Standalone metrics dashboard: latency charts, error rates, request log table.
pub const METRICS_HTML: &str = include_str!("templates/metrics.html");

/// Size of the wizard template in bytes (sanity check < 50 KB).
pub const WIZARD_HTML_LEN: usize = WIZARD_HTML.len();

/// Size of the metrics template in bytes.
pub const METRICS_HTML_LEN: usize = METRICS_HTML.len();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_html_not_empty() {
        assert!(!WIZARD_HTML.is_empty());
        assert!(WIZARD_HTML.starts_with("<!DOCTYPE html>"));
    }

    #[test]
    fn metrics_html_not_empty() {
        assert!(!METRICS_HTML.is_empty());
        assert!(METRICS_HTML.starts_with("<!DOCTYPE html>"));
    }

    #[test]
    fn wizard_size_budget() {
        // Must stay reasonably small for fast loading. The device-login
        // (kiro/antigravity) flow, the Free/All model toggle, the Fetched /
        // Built-in sub-tabs, the custom Promise-based dialogs, and the
        // no-whole-page-scroll layout all added necessary markup + JS. The
        // antigravity PKCE browser-login flow (Open Google Login button +
        // account-poll) pushed it just over 62 KB, so the budget was raised to
        // 66 KB. The Router Access card (API-key enable/disable + key-display
        // modal) added the router auth UI, raising it again to 72 KB. The
        // central Keys tab (combined per-provider key list with reveal / copy /
        // regenerate / delete, all persisting via the verified POST /api/config
        // flow) raised it to 78 KB. The wizard UX lane added optimistic key-add
        // (payload built without mutating S), a dirty-check before remote
        // sync, a central provider-select "Add key" button, 50-per-page model
        // pagination, and server-side key rotation on model fetch, raising it
        // to 80 KB. The error-details debug toggle (See details / hide details
        // with readonly textarea, copy-to-clipboard, full HTTP status + response
        // body on every error path) raised it to 85 KB. Device-login providers
        // (kiro/antigravity) now appear in Model Discovery and Tier Editor
        // dropdowns with device-account labels, raising it to 90 KB.
        assert!(
            WIZARD_HTML_LEN < 90_000,
            "wizard.html is {} bytes, exceeds 90 KB budget",
            WIZARD_HTML_LEN
        );
    }

    #[test]
    fn metrics_size_budget() {
        assert!(
            METRICS_HTML_LEN < 50_000,
            "metrics.html is {} bytes, exceeds 50 KB budget",
            METRICS_HTML_LEN
        );
    }

    #[test]
    fn wizard_has_required_sections() {
        assert!(WIZARD_HTML.contains("providerGrid"), "missing provider selector");
        assert!(WIZARD_HTML.contains("keyList"), "missing key list");
        assert!(WIZARD_HTML.contains("banDuration"), "missing ban config");
        assert!(WIZARD_HTML.contains("modelList"), "missing model list");
        assert!(WIZARD_HTML.contains("tierList"), "missing tier editor");
        assert!(WIZARD_HTML.contains("latencyChart"), "missing latency chart");
        assert!(WIZARD_HTML.contains("errorChart"), "missing error chart");
    }

    #[test]
    fn eye_icon_is_inline_svg() {
        // Eye icon should be inline SVG, not external
        assert!(WIZARD_HTML.contains("EYE_OPEN"), "missing eye-open SVG constant");
        assert!(WIZARD_HTML.contains("EYE_CLOSED"), "missing eye-closed SVG constant");
        assert!(WIZARD_HTML.contains("<svg viewBox"), "eye icon must be inline SVG");
    }

    #[test]
    fn no_external_cdn() {
        assert!(
            !WIZARD_HTML.contains("cdn."),
            "must not reference external CDN"
        );
        assert!(
            !WIZARD_HTML.contains("src=\"http"),
            "must not load external scripts"
        );
    }

    #[test]
    fn has_toggle_mechanism() {
        assert!(WIZARD_HTML.contains("type=\"password\"") || WIZARD_HTML.contains("masked"),
            "must have password/masked field");
        assert!(WIZARD_HTML.contains("type=\"text\"") || WIZARD_HTML.contains("visible"),
            "must have text/visible toggle");
    }
}
