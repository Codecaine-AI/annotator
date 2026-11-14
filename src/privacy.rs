//! What app state may keep. Every collector goes through here before it stores a string: the
//! click and mark lookups (`crate::ax`), the window log and the capture's window title
//! (`crate::windows`, `crate::capture`), dev-page events (`crate::page`) and app shots
//! (`crate::appshot`).
//!
//! The rules:
//! - A value is kept only for static text. A text field, text area, combo box, search field or
//!   secure field keeps its role and name, and its value and everything under it are dropped:
//!   text under an input is that input's value.
//! - Every string is one line of at most `MAX_CHARS`, and a URL inside it loses its query, its
//!   fragment and its `user:password@`, with a scheme or without one.
//! - A password manager (`SENSITIVE_APPS`) gives its app name and nothing else.
//! - A browser's own pages (`chrome://`, `about:`) give no tree.
//!
//! Text an app shows as static text (a code in a message, say) is kept: it is on the keyframe
//! image too.

/// The longest string app state keeps.
pub const MAX_CHARS: usize = 200;

/// Apps whose windows are secrets, by exact bundle id (read with `mdls -name
/// kMDItemCFBundleIdentifier`): 1Password, Bitwarden, Keychain Access, Passwords and LastPass.
pub const SENSITIVE_APPS: [&str; 5] = [
    "com.1password.1password",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.lastpass.LastPass",
];

/// URL schemes of a browser's own pages (settings, history, saved passwords).
const INTERNAL_URLS: [&str; 4] = ["chrome://", "edge://", "about:", "safari-resource:"];

/// A sensitive app gives no title, URL, tree, label, identifier or document.
pub fn is_sensitive_app(bundle_id: Option<&str>) -> bool {
    bundle_id.is_some_and(|id| SENSITIVE_APPS.contains(&id))
}

/// A browser's own page gives no tree.
pub fn is_internal_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    INTERNAL_URLS.iter().any(|scheme| url.starts_with(scheme))
}

/// Roles whose value is what the user typed. Their value is never read, nor anything under them.
pub fn is_input(role: &str, subrole: &str) -> bool {
    subrole == "AXSecureTextField"
        || matches!(
            role,
            "AXSecureTextField" | "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
        )
}

/// Whether an element's `AXValue` may be read at all: only static text shows its value as
/// text on screen. Ask this before the read, so a typed value never leaves its app.
pub fn reads_value(role: &str, subrole: &str) -> bool {
    role == "AXStaticText" && !is_input(role, subrole)
}

/// One element as the Accessibility tree gave it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub role: String,
    pub subrole: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub value: Option<String>,
}

/// The node as it may be stored, and whether what is under it may be read. An input keeps its
/// role and its names (title, description and identifier, which the app's developer wrote),
/// and nothing under it is read.
pub fn node(raw: Node) -> (Node, bool) {
    let input = is_input(&raw.role, &raw.subrole);
    let keep = |s: Option<String>| s.as_deref().and_then(text);
    let value = if reads_value(&raw.role, &raw.subrole) {
        keep(raw.value)
    } else {
        None
    };
    (
        Node {
            title: keep(raw.title),
            description: keep(raw.description),
            identifier: keep(raw.identifier),
            value,
            role: raw.role,
            subrole: raw.subrole,
        },
        !input,
    )
}

/// Read access to an Accessibility tree. The click and mark lookups (`crate::ax`) and app shots
/// (`crate::appshot`) read through it, so tests run them over a fake tree that records every
/// attribute asked for.
pub trait Tree {
    type Element: Clone;
    /// A string attribute; `None` when it is missing, empty or not a string.
    fn string(&self, element: &Self::Element, attribute: &'static str) -> Option<String>;
    /// A URL attribute (`AXURL`, `AXDocument`) as text.
    fn url(&self, element: &Self::Element, attribute: &'static str) -> Option<String>;
    /// An element attribute (`AXParent`, `AXWindow`).
    fn element(&self, element: &Self::Element, attribute: &'static str) -> Option<Self::Element>;
    fn children(&self, element: &Self::Element) -> Vec<Self::Element>;
    /// The same number for the same element, to walk each one once.
    fn key(&self, element: &Self::Element) -> u64;
}

/// Reads an element's names (its `role` and `subrole` are read already) and, for static text
/// only, its value. This is the one place that asks for `AXValue`. `stop` is checked before each
/// read; `None` when it fired. Returns the node as it may be stored and whether what is under
/// it may be read (`node`).
pub fn read<T: Tree>(
    tree: &T,
    element: &T::Element,
    role: &str,
    subrole: &str,
    stop: &dyn Fn() -> bool,
) -> Option<(Node, bool)> {
    let mut raw = Node {
        role: role.to_owned(),
        subrole: subrole.to_owned(),
        ..Node::default()
    };
    for (attribute, slot) in [
        ("AXTitle", &mut raw.title),
        ("AXDescription", &mut raw.description),
        ("AXIdentifier", &mut raw.identifier),
    ] {
        if stop() {
            return None;
        }
        *slot = tree.string(element, attribute);
    }
    if reads_value(role, subrole) {
        if stop() {
            return None;
        }
        raw.value = tree.string(element, "AXValue");
    }
    Some(node(raw))
}

/// For a point lookup: the nearest input in `chain`, which holds the `(role, subrole)` of the
/// element under the point first and then of each ancestor up to its window. Text under an
/// input is that input's value, so the lookup names the input and reads no value.
pub fn input_over(chain: &[(String, String)]) -> Option<usize> {
    chain
        .iter()
        .position(|(role, subrole)| is_input(role, subrole))
}

/// One line of at most `max` characters (never more than `MAX_CHARS`), URLs in it without
/// their query, fragment and login (`bare_token`). `None` when nothing is left.
pub fn clip(raw: &str, max: usize) -> Option<String> {
    let max = max.clamp(1, MAX_CHARS);
    let compact = raw
        .split_whitespace()
        .map(bare_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        return None;
    }
    Some(if compact.chars().count() <= max {
        compact
    } else {
        let cut: String = compact.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    })
}

/// `clip` at `MAX_CHARS`: any string app state stores.
pub fn text(raw: &str) -> Option<String> {
    clip(raw, MAX_CHARS)
}

/// What may stand in front of a URL in a word: `(a.dev/x)`.
const OPENERS: [char; 9] = ['(', '[', '{', '<', '"', '\'', '“', '‘', '«'];
/// What may end a word of prose after its `?` or `#`: `(why?)`.
const CLOSERS: [char; 16] = [
    ')', ']', '}', '>', '"', '\'', '”', '’', '»', '.', ',', ';', ':', '!', '?', '…',
];

/// A word without the secrets of a URL in it: its query, its fragment and its login.
///
/// A `?` or `#` after a scheme (`https://a.dev/x?token=1#top`) always starts one. Anywhere else
/// it starts one when something follows it and either that holds a `=` (`?token=1`) or the word
/// up to there reads as an address (`url_like`: `page.html?1`, `a.dev/x#1`). So "why?", "#42"
/// and "C#" stay.
fn bare_token(token: &str) -> String {
    let scheme = token.find("://");
    let cut = token.char_indices().find(|&(at, c)| {
        matches!(c, '?' | '#')
            && (scheme.is_some_and(|scheme| scheme < at)
                || is_query(&token[..at], &token[at + 1..]))
    });
    without_login(cut.map_or(token, |(at, _)| &token[..at]))
}

/// Whether the `?` or `#` between `before` and `after` starts a query or a fragment.
fn is_query(before: &str, after: &str) -> bool {
    after.chars().any(|c| !CLOSERS.contains(&c)) && (after.contains('=') || url_like(before))
}

/// Whether a word reads as an address without a scheme: it has a path (`/reset`), a dotted
/// name (`a.dev`, `page.html`) or a port (`localhost:3000`).
fn url_like(word: &str) -> bool {
    let chars: Vec<char> = word.trim_start_matches(OPENERS).chars().collect();
    chars.contains(&'/')
        || chars.windows(3).any(|w| {
            w[0].is_alphanumeric()
                && ((w[1] == '.' && w[2].is_alphanumeric())
                    || (w[1] == ':' && w[2].is_ascii_digit()))
        })
}

/// Where the host of the address in a word starts, and whether whatever stands before an `@`
/// there is a login.
///
/// The host is behind the slashes that open the address, however many there are: browsers take
/// `https:/a.dev`, `https:///a.dev` and backslashes for `https://a.dev`, and `//a.dev` takes
/// its scheme from the page. Only a file has no host there: its path starts at the third
/// slash. After such an opening anything before `@` is a login (`https://token@a.dev`).
/// Without one only `user:password@` is: `kit@a.dev` is an address.
fn host_start(word: &str) -> (usize, bool) {
    let slashes = |at: usize| {
        word[at..]
            .chars()
            .take_while(|c| matches!(c, '/' | '\\'))
            .count()
    };
    if let Some(colon) = word.find(":/").or_else(|| word.find(":\\")) {
        let run = slashes(colon + 1);
        let file = word[..colon].to_ascii_lowercase().ends_with("file");
        return (colon + 1 + if file { run.min(2) } else { run }, true);
    }
    let lead = word.len() - word.trim_start_matches(OPENERS).len();
    match slashes(lead) {
        0 | 1 => (lead, false),
        run => (lead + run, true),
    }
}

/// A word without the login in front of its host (`host_start`).
fn without_login(word: &str) -> String {
    let (host_at, any) = host_start(word);
    let host_end = word[host_at..]
        .find(['/', '\\'])
        .map_or(word.len(), |at| host_at + at);
    match word[host_at..host_end].rfind('@') {
        Some(at) if any || word[host_at..host_at + at].contains(':') => {
            format!("{}{}", &word[..host_at], &word[host_at + at + 1..])
        }
        _ => word.to_owned(),
    }
}

/// A URL as it may be stored: no query, no fragment, no `user:password@` (`clip` takes the
/// login off).
pub fn url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    clip(raw.split(['?', '#']).next().unwrap_or(raw), MAX_CHARS)
}

/// A URL as it may be stored or logged: no query, no fragment, no login. Every way to make one
/// goes through `url`, reading one from JSON included, so a field of this type cannot hold a
/// secret a page put in its address. Empty when nothing was left of what it was made from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(from = "String", into = "String")]
pub struct CleanUrl(String);

impl CleanUrl {
    /// `None` when nothing is left of `raw`.
    pub fn new(raw: &str) -> Option<Self> {
        url(raw).map(Self)
    }
}

impl From<&str> for CleanUrl {
    fn from(raw: &str) -> Self {
        Self::new(raw).unwrap_or_default()
    }
}

impl From<String> for CleanUrl {
    fn from(raw: String) -> Self {
        Self::from(raw.as_str())
    }
}

impl From<CleanUrl> for String {
    fn from(url: CleanUrl) -> Self {
        url.0
    }
}

impl std::ops::Deref for CleanUrl {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for CleanUrl {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl std::fmt::Display for CleanUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A window's title as the window log and the capture's target may keep it: empty for a
/// sensitive app.
pub fn window_title(bundle_id: Option<&str>, title: &str) -> String {
    if is_sensitive_app(bundle_id) {
        return String::new();
    }
    text(title).unwrap_or_default()
}

/// An Accessibility tree for tests: elements are indexes, every read is logged, and each read
/// can take time (a slow or hung app).
/// A capture's target as a bundle may keep it: its window title goes through `window_title`.
pub fn target(target: &mut crate::model::TargetInfo) {
    target.window_title = target
        .window_title
        .take()
        .map(|title| window_title(target.bundle_id.as_deref(), &title))
        .filter(|title| !title.is_empty());
}

#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    #[derive(Default)]
    pub struct FakeTree {
        attributes: Vec<HashMap<&'static str, String>>,
        parents: Vec<Option<usize>>,
        children: Vec<Vec<usize>>,
        /// Every `(element, attribute)` asked for, in order.
        pub reads: RefCell<Vec<(usize, &'static str)>>,
        /// How long each read takes.
        pub delay: Duration,
    }

    impl FakeTree {
        /// Adds an element under `parent` and returns it.
        pub fn add(&mut self, parent: Option<usize>, attributes: &[(&'static str, &str)]) -> usize {
            let id = self.attributes.len();
            self.attributes.push(
                attributes
                    .iter()
                    .map(|(name, value)| (*name, (*value).to_owned()))
                    .collect(),
            );
            self.parents.push(parent);
            self.children.push(Vec::new());
            if let Some(parent) = parent {
                self.children[parent].push(id);
            }
            id
        }

        /// Makes `child` a child of `parent` too (a tree that loops).
        pub fn link(&mut self, parent: usize, child: usize) {
            self.children[parent].push(child);
        }

        /// Whether `attribute` was ever asked of `element`.
        pub fn was_read(&self, element: usize, attribute: &str) -> bool {
            self.reads
                .borrow()
                .iter()
                .any(|(e, a)| *e == element && *a == attribute)
        }

        fn note(&self, element: usize, attribute: &'static str) {
            self.reads.borrow_mut().push((element, attribute));
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
        }
    }

    impl super::Tree for FakeTree {
        type Element = usize;

        fn string(&self, element: &usize, attribute: &'static str) -> Option<String> {
            self.note(*element, attribute);
            self.attributes[*element]
                .get(attribute)
                .filter(|v| !v.trim().is_empty())
                .cloned()
        }

        fn url(&self, element: &usize, attribute: &'static str) -> Option<String> {
            self.string(element, attribute)
        }

        fn element(&self, element: &usize, attribute: &'static str) -> Option<usize> {
            self.note(*element, attribute);
            match attribute {
                "AXParent" => self.parents[*element],
                "AXWindow" => {
                    let mut at = Some(*element);
                    while let Some(id) = at {
                        if self.attributes[id].get("AXRole").map(String::as_str) == Some("AXWindow")
                        {
                            return Some(id);
                        }
                        at = self.parents[id];
                    }
                    None
                }
                _ => None,
            }
        }

        fn children(&self, element: &usize) -> Vec<usize> {
            self.note(*element, "AXChildren");
            self.children[*element].clone()
        }

        fn key(&self, element: &usize) -> u64 {
            *element as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "SECRET-MARKER";

    #[test]
    fn privacy_never_asks_an_input_for_its_value() {
        let mut tree = fake::FakeTree::default();
        let field = tree.add(
            None,
            &[
                ("AXRole", "AXTextField"),
                ("AXTitle", "Name"),
                ("AXValue", "SECRET-MARKER-INPUT"),
            ],
        );
        let secure = tree.add(
            None,
            &[
                ("AXRole", "AXTextField"),
                ("AXSubrole", "AXSecureTextField"),
                ("AXValue", "SECRET-MARKER-INPUT"),
            ],
        );
        let label = tree.add(
            None,
            &[("AXRole", "AXStaticText"), ("AXValue", "Open the settings")],
        );
        let never = || false;
        let (kept, descend) = read(&tree, &field, "AXTextField", "", &never).unwrap();
        assert_eq!((kept.title.as_deref(), kept.value, descend), (Some("Name"), None, false));
        let (kept, descend) =
            read(&tree, &secure, "AXTextField", "AXSecureTextField", &never).unwrap();
        assert_eq!((kept.value, descend), (None, false));
        let (kept, descend) = read(&tree, &label, "AXStaticText", "", &never).unwrap();
        assert_eq!((kept.value.as_deref(), descend), (Some("Open the settings"), true));
        assert!(!tree.was_read(field, "AXValue"), "a text field's value is never read");
        assert!(!tree.was_read(secure, "AXValue"), "a secure field's value is never read");
        assert!(tree.was_read(label, "AXValue"));
        // A read that is stopped gives nothing rather than half a node.
        assert!(read(&tree, &label, "AXStaticText", "", &|| true).is_none());
    }

    fn raw(role: &str, subrole: &str) -> Node {
        Node {
            role: role.into(),
            subrole: subrole.into(),
            title: Some("Name".into()),
            description: Some("Your name".into()),
            identifier: Some("name-field".into()),
            value: Some(format!("{MARKER}-INPUT")),
        }
    }

    #[test]
    fn privacy_keeps_a_value_only_for_static_text() {
        let (kept, descend) = node(Node {
            value: Some("Your code is 481 516".into()),
            ..raw("AXStaticText", "")
        });
        assert_eq!(kept.value.as_deref(), Some("Your code is 481 516"));
        assert!(descend);
        for role in [
            "AXButton",
            "AXGroup",
            "AXLink",
            "AXCell",
            "AXPopUpButton",
            "AXSlider",
            "AXWebArea",
            "AXHeading",
        ] {
            let (kept, descend) = node(raw(role, ""));
            assert_eq!(kept.value, None, "{role} keeps no value");
            assert_eq!(kept.title.as_deref(), Some("Name"));
            assert!(descend, "{role} is walked into");
            assert!(!reads_value(role, ""), "{role}'s value is never read");
        }
        assert!(reads_value("AXStaticText", ""));
    }

    #[test]
    fn privacy_drops_an_inputs_value_and_everything_under_it() {
        for (role, subrole) in [
            ("AXTextField", ""),
            ("AXTextArea", ""),
            ("AXComboBox", ""),
            ("AXSearchField", ""),
            ("AXSecureTextField", ""),
            ("AXTextField", "AXSecureTextField"),
            // A secure field that calls itself static text is still a secure field.
            ("AXStaticText", "AXSecureTextField"),
        ] {
            assert!(is_input(role, subrole));
            assert!(!reads_value(role, subrole), "{role}/{subrole}");
            let (kept, descend) = node(raw(role, subrole));
            assert_eq!(kept.value, None, "{role}/{subrole} keeps no value");
            assert_eq!(kept.role, role);
            assert_eq!(kept.title.as_deref(), Some("Name"));
            assert!(!descend, "nothing under {role}/{subrole} is read");
            assert!(!format!("{kept:?}").contains(MARKER));
        }
        assert!(!is_input("AXStaticText", ""));
        assert!(!is_input("AXButton", ""));
    }

    #[test]
    fn privacy_names_the_input_when_the_point_is_on_text_inside_it() {
        let chain = |roles: &[(&str, &str)]| -> Vec<(String, String)> {
            roles
                .iter()
                .map(|(r, s)| ((*r).to_owned(), (*s).to_owned()))
                .collect()
        };
        // The typed text of a web text field is a static text under it.
        assert_eq!(
            input_over(&chain(&[
                ("AXStaticText", ""),
                ("AXGroup", ""),
                ("AXTextField", ""),
                ("AXWebArea", ""),
                ("AXWindow", ""),
            ])),
            Some(2)
        );
        assert_eq!(
            input_over(&chain(&[
                ("AXStaticText", ""),
                ("AXTextField", "AXSecureTextField"),
                ("AXWindow", ""),
            ])),
            Some(1)
        );
        assert_eq!(input_over(&chain(&[("AXTextArea", "")])), Some(0));
        assert_eq!(
            input_over(&chain(&[
                ("AXStaticText", ""),
                ("AXButton", ""),
                ("AXWindow", "")
            ])),
            None
        );
    }

    #[test]
    fn privacy_cuts_every_string_to_one_short_line() {
        assert_eq!(text("  New\n  Tab "), Some("New Tab".into()));
        assert_eq!(text("   "), None);
        let clipped = text(&"x".repeat(500)).unwrap();
        assert_eq!(clipped.chars().count(), MAX_CHARS);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip(&"x".repeat(500), 60).unwrap().chars().count(), 60);
        assert_eq!(
            clip(&"x".repeat(500), 10_000).unwrap().chars().count(),
            MAX_CHARS,
            "no caller can ask for more than the cap"
        );
        let (kept, _) = node(Node {
            title: Some("t".repeat(900)),
            description: Some("d".repeat(900)),
            identifier: Some("i".repeat(900)),
            value: Some("v".repeat(900)),
            ..raw("AXStaticText", "")
        });
        for field in [kept.title, kept.description, kept.identifier, kept.value] {
            assert_eq!(field.unwrap().chars().count(), MAX_CHARS);
        }
    }

    #[test]
    fn privacy_takes_the_query_and_fragment_off_urls_inside_text() {
        let cases = [
            (
                "Fixture https://a.dev/reset?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG done",
                "Fixture https://a.dev/reset done",
            ),
            (
                "file:///Users/k/appshot-secret.html?token=SECRET-MARKER-URL",
                "file:///Users/k/appshot-secret.html",
            ),
            (
                "appshot-secret.html?token=SECRET-MARKER-URL",
                "appshot-secret.html",
            ),
            (
                "a.dev/callback#access_token=SECRET-MARKER-URL",
                "a.dev/callback",
            ),
            (
                "(see https://a.dev/x?k=SECRET-MARKER-URL)",
                "(see https://a.dev/x",
            ),
            ("http://localhost:3000/#SECRET-MARKER", "http://localhost:3000/"),
        ];
        for (raw, kept) in cases {
            let out = text(raw).unwrap();
            assert_eq!(out, kept);
            assert!(!out.contains(MARKER), "{out}");
        }
        // Ordinary punctuation is not a URL.
        for plain in ["Why? Because a=b.", "Issue #42 is fixed", "What is 2+2=?"] {
            assert_eq!(text(plain).as_deref(), Some(plain));
        }
    }

    #[test]
    fn privacy_text_strips_every_login_query_and_fragment_scheme_or_not() {
        let cases = [
            // A login, with a scheme: whatever stands before `@`.
            (
                "Reset at https://user:SECRET-MARKER@example.test/reset now",
                "Reset at https://example.test/reset now",
            ),
            (
                "https://SECRET-MARKER@example.test/reset",
                "https://example.test/reset",
            ),
            ("(https://kit:SECRET-MARKER@a.dev/x)", "(https://a.dev/x)"),
            // A login without a scheme is `user:password@`.
            (
                "user:SECRET-MARKER@example.test/reset",
                "example.test/reset",
            ),
            ("(kit:SECRET-MARKER@a.dev/x)", "(a.dev/x)"),
            // A fragment or a query with no `=`, without a scheme.
            (
                "example.test/callback#SECRET-MARKER",
                "example.test/callback",
            ),
            ("a.dev/reset#SECRET-MARKER", "a.dev/reset"),
            ("reset.html?SECRET-MARKER", "reset.html"),
            ("/reset?SECRET-MARKER", "/reset"),
            ("localhost:3000?SECRET-MARKER", "localhost:3000"),
            ("Open ?token=SECRET-MARKER now", "Open now"),
            ("Go #access_token=SECRET-MARKER", "Go"),
            // A query in front of a URL.
            ("next?to=https://a.dev/x&token=SECRET-MARKER", "next"),
            (
                "https://a.dev/x?next=/home&mail=SECRET-MARKER@b.dev",
                "https://a.dev/x",
            ),
        ];
        for (raw, kept) in cases {
            let out = text(raw).unwrap();
            assert_eq!(out, kept, "{raw}");
            assert!(!out.contains(MARKER), "{out}");
        }
        assert_eq!(text("?token=SECRET-MARKER"), None);
        // Ordinary prose stays as it is written.
        for plain in [
            "Is the build done?",
            "Fix #1 first",
            "See step #1.",
            "Really?!",
            "Is it in config.json?",
            "(why?)",
            "“Ready?” she said",
            "C# and F#",
            "and/or?",
            "Mail kit@a.dev",
            "git@github.com:kit/annotator.git",
            "[#12](https://a.dev/pull/12)",
            "src/App.tsx:4",
        ] {
            assert_eq!(text(plain).as_deref(), Some(plain));
        }
    }

    #[test]
    fn privacy_text_strips_a_login_however_the_address_starts() {
        let cases = [
            // An address that takes its scheme from the page.
            ("//user:SECRET-MARKER@a.dev/x", "//a.dev/x"),
            ("(//user:SECRET-MARKER@a.dev/x)", "(//a.dev/x)"),
            ("//SECRET-MARKER@a.dev", "//a.dev"),
            ("See //user:SECRET-MARKER@a.dev/x?next=1 now", "See //a.dev/x now"),
            // Browsers take a scheme with one slash, with three, and with backslashes.
            ("https:/user:SECRET-MARKER@a.dev/x", "https:/a.dev/x"),
            ("https:///user:SECRET-MARKER@a.dev/x", "https:///a.dev/x"),
            ("https:\\\\user:SECRET-MARKER@a.dev\\x", "https:\\\\a.dev\\x"),
            ("///user:SECRET-MARKER@a.dev/x", "///a.dev/x"),
            // A file server's login; a file's own path is not a host.
            ("file://kit:SECRET-MARKER@server/share", "file://server/share"),
        ];
        for (raw, kept) in cases {
            let out = text(raw).unwrap();
            assert_eq!(out, kept, "{raw}");
            assert!(!out.contains(MARKER), "{out}");
        }
        assert_eq!(
            url("//user:SECRET-MARKER@a.dev/x?token=SECRET-MARKER-URL").as_deref(),
            Some("//a.dev/x")
        );
        for plain in [
            "// a comment",
            "file:///Users/k/a@b/x.md",
            "PATH:/usr/local/bin",
            "C:\\Users\\kit",
            "either/or",
            "git@github.com:/kit/annotator.git",
        ] {
            assert_eq!(text(plain).as_deref(), Some(plain));
        }
    }

    #[test]
    fn privacy_stores_urls_without_query_fragment_or_login() {
        assert_eq!(
            url("https://a.dev/reset?token=SECRET-MARKER-URL#frag").as_deref(),
            Some("https://a.dev/reset")
        );
        assert_eq!(
            url("https://kit:SECRET-MARKER@a.dev/x").as_deref(),
            Some("https://a.dev/x")
        );
        assert_eq!(
            url("file:///Users/k/My%20Notes.md#SECRET-MARKER").as_deref(),
            Some("file:///Users/k/My%20Notes.md")
        );
        assert_eq!(url("  "), None);
        assert!(url(&format!("https://a.dev/{}", "p".repeat(900))).unwrap().chars().count() <= MAX_CHARS);
    }

    #[test]
    fn privacy_gives_nothing_but_the_app_for_a_password_manager() {
        for id in SENSITIVE_APPS {
            assert!(is_sensitive_app(Some(id)));
            assert_eq!(window_title(Some(id), "Login: SECRET-MARKER vault"), "");
        }
        // Exact ids only: a look-alike is an ordinary app.
        assert!(!is_sensitive_app(Some("com.1password.1password.helper")));
        assert!(!is_sensitive_app(Some("com.apple.Safari")));
        assert!(!is_sensitive_app(None));
        assert_eq!(
            window_title(Some("com.apple.Safari"), "Fixture https://a.dev/?t=SECRET-MARKER"),
            "Fixture https://a.dev/"
        );
        assert_eq!(window_title(None, "  Notes  "), "Notes");
    }

    #[test]
    fn privacy_cleans_a_capture_targets_window_title() {
        let title = "Fixture https://a.dev/reset?token=SECRET-MARKER-URL";
        let make = |bundle_id: &str| crate::model::TargetInfo {
            mode: crate::model::CaptureMode::Window,
            app: Some("App".into()),
            bundle_id: Some(bundle_id.into()),
            window_title: Some(title.into()),
            window_id: Some(7),
            display_id: 1,
            frame: crate::model::TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
                scale: 2.0,
            },
        };
        let mut browser = make("com.apple.Safari");
        target(&mut browser);
        assert_eq!(
            browser.window_title.as_deref(),
            Some("Fixture https://a.dev/reset")
        );
        let mut vault = make("com.1password.1password");
        target(&mut vault);
        assert_eq!(vault.window_title, None, "a password manager's title is dropped");
        assert_eq!(vault.app.as_deref(), Some("App"));
    }

    #[test]
    fn privacy_reads_no_tree_from_a_browsers_own_pages() {
        for internal in [
            "chrome://settings/passwords",
            "edge://settings",
            "about:blank",
            "safari-resource:/ErrorPage.html",
            "Chrome://History",
        ] {
            assert!(is_internal_url(internal), "{internal}");
        }
        for page in ["https://a.dev/about:", "file:///a.html", "http://chrome.dev"] {
            assert!(!is_internal_url(page), "{page}");
        }
    }
}
