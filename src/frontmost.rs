//! Detects which AI coding app is in front, for automatic view switching.
//!
//! Only the bundle identifier of the frontmost application is read; this needs
//! no Accessibility permission. Terminals, this monitor itself and unrelated
//! apps return `None` so the caller keeps its previous view.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Assistant {
    Codex,
    Claude,
}

pub fn classify_bundle_id(bundle_id: &str) -> Option<Assistant> {
    match bundle_id {
        "com.openai.codex" | "com.openai.chat" => Some(Assistant::Codex),
        id if id.starts_with("com.anthropic.claude") => Some(Assistant::Claude),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
pub fn frontmost_assistant() -> Option<Assistant> {
    use objc2_app_kit::NSWorkspace;

    let application = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    if i64::from(application.processIdentifier()) == i64::from(std::process::id()) {
        return None;
    }
    classify_bundle_id(&application.bundleIdentifier()?.to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_assistant() -> Option<Assistant> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_codex_and_claude_apps_and_ignores_others() {
        assert_eq!(
            classify_bundle_id("com.openai.codex"),
            Some(Assistant::Codex)
        );
        assert_eq!(
            classify_bundle_id("com.openai.chat"),
            Some(Assistant::Codex)
        );
        assert_eq!(
            classify_bundle_id("com.anthropic.claudefordesktop"),
            Some(Assistant::Claude)
        );
        assert_eq!(classify_bundle_id("com.apple.Terminal"), None);
        assert_eq!(classify_bundle_id("com.anthropic.other"), None);
    }
}
