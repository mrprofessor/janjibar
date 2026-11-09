//! Asset source: our icons first, then gpui-kit's bundled icons.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};
use model::resource::Kind;

/// Declares the bundled icons once: the lookup table and the [`Icon`] enum.
macro_rules! icons {
    ($($variant:ident => $file:literal,)*) => {
        /// Icons the app ships itself, from Lucide (ISC, see `assets/icons/LICENSE`).
        const ICONS: &[(&str, &[u8])] = &[
            $((
                concat!("icons/app/", $file, ".svg"),
                include_bytes!(concat!("../assets/icons/", $file, ".svg")),
            ),)*
        ];

        /// An icon path for `svg().path(...)`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($variant,)*
        }

        impl Icon {
            /// Every icon.
            pub const ALL: &[Icon] = &[$(Icon::$variant,)*];

            /// The asset path.
            pub fn path(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("icons/app/", $file, ".svg"),)*
                }
            }
        }
    };
}

icons! {
    Archive => "archive",
    Bell => "bell",
    Box => "box",
    Briefcase => "briefcase",
    CalendarClock => "calendar-clock",
    ChevronDown => "chevron-down",
    ChevronUp => "chevron-up",
    CircleAlert => "circle-alert",
    Close => "x",
    Copy => "copy",
    Cylinder => "cylinder",
    Database => "database",
    FileText => "file-text",
    Folder => "folder",
    FolderOpen => "folder-open",
    Globe => "globe",
    HardDrive => "hard-drive",
    KeyRound => "key-round",
    Layers => "layers",
    LayoutDashboard => "layout-dashboard",
    LoaderCircle => "loader-circle",
    Logs => "logs",
    Network => "network",
    Plug => "plug",
    Search => "search",
    Server => "server",
    ServerCog => "server-cog",
    TriangleAlert => "triangle-alert",
    Unplug => "unplug",
}

/// The icon for a kind, in the sidebar, tabs and inspector.
pub fn kind_icon(kind: Kind) -> Icon {
    match kind {
        Kind::Pod => Icon::Box,
        Kind::Deployment => Icon::Layers,
        Kind::StatefulSet => Icon::Database,
        Kind::DaemonSet => Icon::ServerCog,
        Kind::ReplicaSet => Icon::Copy,
        Kind::Job => Icon::Briefcase,
        Kind::CronJob => Icon::CalendarClock,
        Kind::Service => Icon::Network,
        Kind::Ingress => Icon::Globe,
        Kind::ConfigMap => Icon::FileText,
        Kind::Secret => Icon::KeyRound,
        Kind::PersistentVolumeClaim => Icon::HardDrive,
        Kind::PersistentVolume => Icon::Cylinder,
        Kind::StorageClass => Icon::Archive,
        Kind::Node => Icon::Server,
        Kind::Namespace => Icon::Folder,
        Kind::Event => Icon::Bell,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_path_is_bundled() {
        for icon in Icon::ALL {
            let loaded = Assets.load(icon.path()).unwrap();
            assert!(loaded.is_some_and(|b| !b.is_empty()), "{icon:?}");
        }
    }

    #[test]
    fn every_kind_has_an_icon() {
        for kind in Kind::ALL {
            assert!(Icon::ALL.contains(&kind_icon(kind)));
        }
    }
}
