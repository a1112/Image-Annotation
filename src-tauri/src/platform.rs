use serde::Serialize;
#[cfg(not(mobile))]
use tauri::WebviewWindow;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeBackdropStatus {
    pub platform: &'static str,
    pub effect: &'static str,
    pub applied: bool,
    pub detail: String,
}

impl NativeBackdropStatus {
    pub fn pending() -> Self {
        Self::new(current_platform(), "Pending", false, "Not applied yet")
    }

    pub fn unavailable(platform: &'static str, detail: impl Into<String>) -> Self {
        Self::new(platform, "Unavailable", false, detail)
    }

    #[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
    fn applied(platform: &'static str, effect: &'static str) -> Self {
        Self::new(platform, effect, true, "Native backdrop applied")
    }

    fn new(
        platform: &'static str,
        effect: &'static str,
        applied: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            platform,
            effect,
            applied,
            detail: detail.into(),
        }
    }
}

#[cfg(not(mobile))]
pub fn configure_window(window: &WebviewWindow) -> NativeBackdropStatus {
    apply_platform_chrome(window);
    apply_native_backdrop(window)
}

pub fn current_platform() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }

    #[cfg(target_os = "macos")]
    {
        "macOS"
    }

    #[cfg(target_os = "linux")]
    {
        "Linux"
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "Unsupported"
    }
}

#[cfg(all(not(mobile), target_os = "windows"))]
fn apply_platform_chrome(window: &WebviewWindow) {
    if let Err(error) = window.set_decorations(false) {
        eprintln!("failed to apply frameless chrome: {error}");
    }

    if let Err(error) = apply_windows_corner_preference(window) {
        eprintln!("failed to apply Windows corner preference: {error}");
    }
}

#[cfg(all(not(mobile), target_os = "macos"))]
fn apply_platform_chrome(window: &WebviewWindow) {
    if let Err(error) = window.set_decorations(false) {
        eprintln!("failed to apply frameless chrome: {error}");
    }
}

#[cfg(all(not(mobile), not(any(target_os = "windows", target_os = "macos"))))]
fn apply_platform_chrome(_window: &WebviewWindow) {}

#[cfg(all(not(mobile), target_os = "windows"))]
fn apply_windows_corner_preference(window: &WebviewWindow) -> Result<(), String> {
    use std::mem::size_of_val;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    let hwnd = window.hwnd().map_err(|err| err.to_string())?;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &DWMWCP_ROUND as *const _ as *const _,
            size_of_val(&DWMWCP_ROUND) as u32,
        )
        .map_err(|err| err.to_string())?;
    }

    Ok(())
}

#[cfg(all(not(mobile), target_os = "windows"))]
fn apply_native_backdrop(window: &WebviewWindow) -> NativeBackdropStatus {
    use window_vibrancy::{apply_blur, apply_mica, apply_tabbed};

    match apply_tabbed(window, Some(true)) {
        Ok(_) => NativeBackdropStatus::applied("Windows", "Tabbed Mica"),
        Err(tabbed_error) => match apply_mica(window, Some(true)) {
            Ok(_) => NativeBackdropStatus::applied("Windows", "Mica"),
            Err(mica_error) => match apply_blur(window, Some((246, 248, 251, 225))) {
                Ok(_) => NativeBackdropStatus::applied("Windows", "Blur"),
                Err(blur_error) => NativeBackdropStatus::unavailable(
                    "Windows",
                    format!(
                        "Tabbed Mica failed: {tabbed_error}; Mica failed: {mica_error}; Blur failed: {blur_error}"
                    ),
                ),
            },
        },
    }
}

#[cfg(all(not(mobile), target_os = "macos"))]
fn apply_native_backdrop(window: &WebviewWindow) -> NativeBackdropStatus {
    use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};

    match apply_vibrancy(
        window,
        NSVisualEffectMaterial::UnderWindowBackground,
        Some(NSVisualEffectState::Active),
        Some(8.0),
    ) {
        Ok(_) => NativeBackdropStatus::applied("macOS", "Vibrancy"),
        Err(error) => NativeBackdropStatus::unavailable("macOS", error.to_string()),
    }
}

#[cfg(all(not(mobile), not(any(target_os = "macos", target_os = "windows"))))]
fn apply_native_backdrop(_window: &WebviewWindow) -> NativeBackdropStatus {
    NativeBackdropStatus::unavailable(current_platform(), "Native backdrop is not supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_status_uses_current_platform() {
        let pending = NativeBackdropStatus::pending();

        assert_eq!(pending.platform, current_platform());
        assert_eq!(pending.effect, "Pending");
        assert!(!pending.applied);
    }
}
