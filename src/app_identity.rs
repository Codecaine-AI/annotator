//! Which Annotator this process is: the live app or Annotator Dev. It is read once, from the
//! bundle id of the `.app` that holds this executable, and every per-app path, port, shortcut and
//! command name comes from it, so the two apps can run at the same time without sharing state.
//!
//! Only the live bundle id gives `Live`. Annotator Dev.app, `target/debug/scrawl` and anything
//! else give `Dev`, so a development build never reads or writes the live app's state. Tests see
//! `Live` (the values the rest of the suite asserts) and test `Dev` through its methods.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::capture::{CARBON_CONTROL_KEY, CARBON_OPTION_KEY, CARBON_SHIFT_KEY};

pub const LIVE_BUNDLE_ID: &str = "ai.codecaine.annotator";
pub const DEV_BUNDLE_ID: &str = "ai.codecaine.annotator.dev";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// `/Applications/Annotator.app`, the app used every day.
    Live,
    /// `~/Applications/Annotator Dev.app` or a build run from `target/`.
    Dev,
}

impl Variant {
    /// The variant for this process, read once.
    pub fn current() -> Self {
        static CURRENT: OnceLock<Variant> = OnceLock::new();
        *CURRENT.get_or_init(|| {
            if cfg!(test) || !cfg!(target_os = "macos") {
                return Self::Live;
            }
            let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
            Self::from_bundle_id(exe.and_then(|exe| bundle_id_of_executable(&exe)).as_deref())
        })
    }

    pub fn from_bundle_id(bundle_id: Option<&str>) -> Self {
        if bundle_id == Some(LIVE_BUNDLE_ID) {
            Self::Live
        } else {
            Self::Dev
        }
    }

    pub fn bundle_id(self) -> &'static str {
        match self {
            Self::Live => LIVE_BUNDLE_ID,
            Self::Dev => DEV_BUNDLE_ID,
        }
    }

    /// The folder name under `~/.config` and `~/Library/Application Support`.
    pub fn state_folder(self) -> &'static str {
        match self {
            Self::Live => "scrawl",
            Self::Dev => "scrawl-dev",
        }
    }

    /// The folder name under `~/Library/Logs` and `~/Movies`.
    pub fn output_folder(self) -> &'static str {
        match self {
            Self::Live => "Scrawl",
            Self::Dev => "Scrawl Dev",
        }
    }

    /// The command-line link name (Settings › Agents and the install script).
    pub fn command_name(self) -> &'static str {
        match self {
            Self::Live => "scrawl",
            Self::Dev => "scrawl-dev",
        }
    }

    /// The ScrawlKit ingest and `sdk.js` server port.
    pub fn ingest_port(self) -> u16 {
        match self {
            Self::Live => 47777,
            Self::Dev => 47778,
        }
    }

    /// The Carbon modifiers of the default Record (S) and Paste (V) shortcuts. The live default
    /// ⌥⇧ also registers ⌃⌥⇧, ⌘⌥⇧ and ⌃⌥⌘, so Dev uses ⌃⌥, which none of those hold.
    pub fn shortcut_modifiers(self) -> u32 {
        match self {
            Self::Live => CARBON_OPTION_KEY | CARBON_SHIFT_KEY,
            Self::Dev => CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
        }
    }

    /// Whether a default shortcut also answers to its aliases. Only the live app has them: Dev's
    /// aliases would be the live app's keys.
    pub fn shortcut_aliases(self) -> bool {
        self == Self::Live
    }

    /// `~/.config/<state folder>/<file>`.
    pub fn config_file(self, home: &Path, file: &str) -> PathBuf {
        home.join(".config").join(self.state_folder()).join(file)
    }

    /// `~/Library/Application Support/<state folder>/captures`.
    pub fn captures_dir(self, home: &Path) -> PathBuf {
        home.join("Library/Application Support")
            .join(self.state_folder())
            .join("captures")
    }

    pub fn log_dir(self, home: &Path) -> PathBuf {
        home.join("Library/Logs").join(self.output_folder())
    }

    pub fn movies_dir(self, home: &Path) -> PathBuf {
        home.join("Movies").join(self.output_folder())
    }
}

/// The bundle id of the `.app` that holds `exe` (a real path), from its `Contents/Info.plist`.
fn bundle_id_of_executable(exe: &Path) -> Option<String> {
    let app = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    bundle_id_in_plist(&plist).map(str::to_owned)
}

/// `CFBundleIdentifier` in an XML property list (`install-app.sh` writes XML).
fn bundle_id_in_plist(plist: &str) -> Option<&str> {
    let (_, rest) = plist.split_once("<key>CFBundleIdentifier</key>")?;
    let (_, rest) = rest.split_once("<string>")?;
    let (id, _) = rest.split_once("</string>")?;
    Some(id.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_live_bundle_id_is_live() {
        assert_eq!(Variant::from_bundle_id(Some(LIVE_BUNDLE_ID)), Variant::Live);
        assert_eq!(Variant::from_bundle_id(Some(DEV_BUNDLE_ID)), Variant::Dev);
        assert_eq!(
            Variant::from_bundle_id(Some("dev.kit.scrawl")),
            Variant::Dev
        );
        assert_eq!(Variant::from_bundle_id(None), Variant::Dev, "target/debug");
    }

    #[test]
    fn live_keeps_its_paths_and_dev_gets_its_own() {
        let home = Path::new("/Users/me");
        let live = Variant::Live;
        let dev = Variant::Dev;
        assert_eq!(
            live.config_file(home, "config.json"),
            Path::new("/Users/me/.config/scrawl/config.json")
        );
        assert_eq!(
            dev.config_file(home, "config.json"),
            Path::new("/Users/me/.config/scrawl-dev/config.json")
        );
        assert_eq!(
            live.captures_dir(home),
            Path::new("/Users/me/Library/Application Support/scrawl/captures")
        );
        assert_eq!(
            dev.captures_dir(home),
            Path::new("/Users/me/Library/Application Support/scrawl-dev/captures")
        );
        assert_eq!(
            live.log_dir(home),
            Path::new("/Users/me/Library/Logs/Scrawl")
        );
        assert_eq!(
            dev.log_dir(home),
            Path::new("/Users/me/Library/Logs/Scrawl Dev")
        );
        assert_eq!(live.movies_dir(home), Path::new("/Users/me/Movies/Scrawl"));
        assert_eq!(
            dev.movies_dir(home),
            Path::new("/Users/me/Movies/Scrawl Dev")
        );
    }

    #[test]
    fn dev_has_its_own_names_and_port() {
        assert_eq!(Variant::Live.bundle_id(), "ai.codecaine.annotator");
        assert_eq!(Variant::Dev.bundle_id(), "ai.codecaine.annotator.dev");
        assert_eq!(Variant::Live.command_name(), "scrawl");
        assert_eq!(Variant::Dev.command_name(), "scrawl-dev");
        assert_eq!(Variant::Live.ingest_port(), 47777);
        assert_eq!(Variant::Dev.ingest_port(), 47778);
    }

    #[test]
    fn the_bundle_id_comes_from_the_app_holding_the_executable() {
        let root = std::env::temp_dir().join(format!("annotator-identity-{}", std::process::id()));
        let app = root.join("Annotator Dev.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<plist><dict>\n  <key>CFBundleExecutable</key>\n  <string>scrawl</string>\n  \
             <key>CFBundleIdentifier</key>\n  <string>ai.codecaine.annotator.dev</string>\n</dict></plist>",
        )
        .unwrap();
        assert_eq!(
            bundle_id_of_executable(&app.join("Contents/MacOS/scrawl")).as_deref(),
            Some(DEV_BUNDLE_ID)
        );
        assert_eq!(
            bundle_id_of_executable(&root.join("target/debug/scrawl")),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
