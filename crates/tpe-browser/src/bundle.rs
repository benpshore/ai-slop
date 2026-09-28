//! The macOS application-bundle layout a CEF-based app must have. CEF is a
//! multi-process framework: the main app bundle embeds the framework and one
//! helper `.app` per process type, and every piece is code-signed separately.
//! This module only computes the expected paths; it does not touch the disk.
//! The helper suffix list mirrors `CEF_HELPER_APP_SUFFIXES` in CEF's
//! `cmake/cef_macros.cmake`; confirm it against the downloaded binary
//! distribution before shipping (see `docs/BROWSER.md`).

/// Name of the framework bundle inside `Contents/Frameworks`.
pub const CEF_FRAMEWORK_NAME: &str = "Chromium Embedded Framework";

/// Helper app suffixes (display suffix, bundle-identifier suffix). The empty
/// pair is the generic helper; the others are the dedicated process types.
pub const HELPER_SUFFIXES: &[(&str, &str)] = &[
    ("", ""),
    (" (Alerts)", ".alerts"),
    (" (GPU)", ".gpu"),
    (" (Plugin)", ".plugin"),
    (" (Renderer)", ".renderer"),
];

/// Hardened-runtime entitlements the helper executables need so Chromium's
/// JIT and the framework's unsigned-memory tricks work under notarisation.
pub const HELPER_ENTITLEMENTS: &[&str] = &[
    "com.apple.security.cs.allow-jit",
    "com.apple.security.cs.allow-unsigned-executable-memory",
    "com.apple.security.cs.disable-library-validation",
];

/// One helper application bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperApp {
    /// Display name, for example `TPE Helper (GPU)`.
    pub name: String,
    /// Bundle identifier, for example `org.example.tpe.helper.gpu`.
    pub bundle_id: String,
    /// Bundle path relative to the main app's directory.
    pub bundle_path: String,
    /// Path of the helper executable relative to the main app's directory.
    pub executable: String,
}

/// Expected layout of `<app_name>.app`, all paths relative to the directory
/// that contains the `.app`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacBundleLayout {
    /// Application name without `.app`.
    pub app_name: String,
    /// Main bundle identifier.
    pub bundle_id: String,
    /// `<app>.app`
    pub app_path: String,
    /// `<app>.app/Contents/MacOS/<app>`
    pub executable: String,
    /// `<app>.app/Contents/Resources`
    pub resources_dir: String,
    /// `<app>.app/Contents/Frameworks`
    pub frameworks_dir: String,
    /// `<app>.app/Contents/Frameworks/Chromium Embedded Framework.framework`
    pub cef_framework: String,
    /// The framework's main binary inside `cef_framework`.
    pub cef_library: String,
    /// Helper apps in `Contents/Frameworks`.
    pub helpers: Vec<HelperApp>,
}

impl MacBundleLayout {
    /// Compute the layout for `app_name` and `bundle_id`.
    pub fn new(app_name: &str, bundle_id: &str) -> Self {
        let app_path = format!("{app_name}.app");
        let frameworks_dir = format!("{app_path}/Contents/Frameworks");
        let cef_framework = format!("{frameworks_dir}/{CEF_FRAMEWORK_NAME}.framework");
        let helpers = HELPER_SUFFIXES
            .iter()
            .map(|(display, id)| {
                let name = format!("{app_name} Helper{display}");
                HelperApp {
                    name: name.clone(),
                    bundle_id: format!("{bundle_id}.helper{id}"),
                    bundle_path: format!("{frameworks_dir}/{name}.app"),
                    executable: format!("{frameworks_dir}/{name}.app/Contents/MacOS/{name}"),
                }
            })
            .collect();
        let executable = format!("{app_path}/Contents/MacOS/{app_name}");
        let resources_dir = format!("{app_path}/Contents/Resources");
        let cef_library = format!("{cef_framework}/{CEF_FRAMEWORK_NAME}");
        Self {
            app_name: app_name.to_string(),
            bundle_id: bundle_id.to_string(),
            app_path,
            executable,
            resources_dir,
            frameworks_dir,
            cef_framework,
            cef_library,
            helpers,
        }
    }

    /// Paths in the order `codesign` must sign them: inside out (framework,
    /// helpers, then the main bundle), because signing a bundle seals the
    /// signatures of everything nested inside it.
    pub fn signing_order(&self) -> Vec<String> {
        let mut order = vec![self.cef_framework.clone()];
        order.extend(self.helpers.iter().map(|h| h.bundle_path.clone()));
        order.push(self.app_path.clone());
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths() {
        let layout = MacBundleLayout::new("TPE", "org.example.tpe");
        assert_eq!(layout.executable, "TPE.app/Contents/MacOS/TPE");
        assert_eq!(
            layout.cef_library,
            "TPE.app/Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework"
        );
        assert_eq!(layout.helpers.len(), 5);
        let gpu = &layout.helpers[2];
        assert_eq!(gpu.name, "TPE Helper (GPU)");
        assert_eq!(gpu.bundle_id, "org.example.tpe.helper.gpu");
        assert_eq!(
            gpu.executable,
            "TPE.app/Contents/Frameworks/TPE Helper (GPU).app/Contents/MacOS/TPE Helper (GPU)"
        );
        assert_eq!(layout.helpers[0].bundle_id, "org.example.tpe.helper");
    }

    #[test]
    fn signing_is_inside_out() {
        let layout = MacBundleLayout::new("TPE", "org.example.tpe");
        let order = layout.signing_order();
        assert_eq!(order.len(), 7);
        assert!(order[0].ends_with(".framework"));
        assert_eq!(order[6], "TPE.app");
        assert!(order[1..6].iter().all(|p| p.contains("Helper")));
    }
}
