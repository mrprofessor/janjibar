//! Asset source: our icons first, then gpui-kit's bundled icons.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

/// Icons the app ships itself, from Lucide (ISC, see `assets/icons/LICENSE`).
const ICONS: [(&str, &[u8]); 18] = [
    (
        "icons/app/bell.svg",
        include_bytes!("../assets/icons/bell.svg"),
    ),
    (
        "icons/app/box.svg",
        include_bytes!("../assets/icons/box.svg"),
    ),
    (
        "icons/app/briefcase.svg",
        include_bytes!("../assets/icons/briefcase.svg"),
    ),
    (
        "icons/app/circle-alert.svg",
        include_bytes!("../assets/icons/circle-alert.svg"),
    ),
    (
        "icons/app/database.svg",
        include_bytes!("../assets/icons/database.svg"),
    ),
    (
        "icons/app/file-text.svg",
        include_bytes!("../assets/icons/file-text.svg"),
    ),
    (
        "icons/app/folder-open.svg",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    (
        "icons/app/globe.svg",
        include_bytes!("../assets/icons/globe.svg"),
    ),
    (
        "icons/app/key-round.svg",
        include_bytes!("../assets/icons/key-round.svg"),
    ),
    (
        "icons/app/layers.svg",
        include_bytes!("../assets/icons/layers.svg"),
    ),
    (
        "icons/app/layout-dashboard.svg",
        include_bytes!("../assets/icons/layout-dashboard.svg"),
    ),
    (
        "icons/app/loader-circle.svg",
        include_bytes!("../assets/icons/loader-circle.svg"),
    ),
    (
        "icons/app/logs.svg",
        include_bytes!("../assets/icons/logs.svg"),
    ),
    (
        "icons/app/network.svg",
        include_bytes!("../assets/icons/network.svg"),
    ),
    (
        "icons/app/plug.svg",
        include_bytes!("../assets/icons/plug.svg"),
    ),
    (
        "icons/app/server.svg",
        include_bytes!("../assets/icons/server.svg"),
    ),
    (
        "icons/app/triangle-alert.svg",
        include_bytes!("../assets/icons/triangle-alert.svg"),
    ),
    (
        "icons/app/unplug.svg",
        include_bytes!("../assets/icons/unplug.svg"),
    ),
];

/// The app's asset source. Pass it to `Application::with_assets`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect();
        names.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(names)
    }
}

/// An icon path for `svg().path(...)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Bell,
    Box,
    Briefcase,
    CircleAlert,
    Database,
    FileText,
    FolderOpen,
    Globe,
    KeyRound,
    Layers,
    LayoutDashboard,
    LoaderCircle,
    Logs,
    Network,
    Plug,
    Server,
    TriangleAlert,
    Unplug,
    // From gpui-kit's bundle, which `Assets` falls back to.
    ChevronDown,
    ChevronUp,
    Close,
    Search,
}

impl Icon {
    /// The asset path.
    pub fn path(self) -> &'static str {
        match self {
            Self::Bell => "icons/app/bell.svg",
            Self::Box => "icons/app/box.svg",
            Self::Briefcase => "icons/app/briefcase.svg",
            Self::CircleAlert => "icons/app/circle-alert.svg",
            Self::Database => "icons/app/database.svg",
            Self::FileText => "icons/app/file-text.svg",
            Self::FolderOpen => "icons/app/folder-open.svg",
            Self::Globe => "icons/app/globe.svg",
            Self::KeyRound => "icons/app/key-round.svg",
            Self::Layers => "icons/app/layers.svg",
            Self::LayoutDashboard => "icons/app/layout-dashboard.svg",
            Self::LoaderCircle => "icons/app/loader-circle.svg",
            Self::Logs => "icons/app/logs.svg",
            Self::Network => "icons/app/network.svg",
            Self::Plug => "icons/app/plug.svg",
            Self::Server => "icons/app/server.svg",
            Self::TriangleAlert => "icons/app/triangle-alert.svg",
            Self::Unplug => "icons/app/unplug.svg",
            Self::ChevronDown => "icons/chevron-down.svg",
            Self::ChevronUp => "icons/chevron-up.svg",
            Self::Close => "icons/close.svg",
            Self::Search => "icons/search.svg",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_path_is_bundled() {
        let all = [
            Icon::Bell,
            Icon::Box,
            Icon::Briefcase,
            Icon::CircleAlert,
            Icon::Database,
            Icon::FileText,
            Icon::FolderOpen,
            Icon::Globe,
            Icon::KeyRound,
            Icon::Layers,
            Icon::LayoutDashboard,
            Icon::LoaderCircle,
            Icon::Logs,
            Icon::Network,
            Icon::Plug,
            Icon::Server,
            Icon::TriangleAlert,
            Icon::Unplug,
            Icon::ChevronDown,
            Icon::ChevronUp,
            Icon::Close,
            Icon::Search,
        ];
        for icon in all {
            let loaded = Assets.load(icon.path()).unwrap();
            assert!(loaded.is_some_and(|b| !b.is_empty()), "{icon:?}");
        }
    }
}
