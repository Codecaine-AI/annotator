//! What's under a click or a mark, from the Accessibility tree (app state, `handoff::app_state`):
//! the element's role, label, identifier and the document its window shows. Lookups run on one
//! background thread with a short messaging timeout, so a hung app never holds up input. What
//! may be read and kept is `crate::privacy`'s to say: a value only for static text, an input's
//! name and never its value or anything under it, nothing but the role in a password manager,
//! and every string short and without a URL's query.

#[cfg(target_os = "macos")]
use core_foundation::base::{CFType, CFTypeRef, TCFType};
#[cfg(target_os = "macos")]
use core_foundation::string::{CFString, CFStringRef};
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::privacy::Tree;

#[cfg(target_os = "macos")]
pub(crate) type AxElement = *const c_void;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> AxElement;
    pub(crate) fn AXUIElementCreateApplication(pid: i32) -> AxElement;
    fn AXUIElementCopyElementAtPosition(
        element: AxElement,
        x: f32,
        y: f32,
        out: *mut AxElement,
    ) -> i32;
    pub(crate) fn AXUIElementCopyAttributeValue(
        element: AxElement,
        attribute: CFStringRef,
        out: *mut CFTypeRef,
    ) -> i32;
    pub(crate) fn AXUIElementSetMessagingTimeout(element: AxElement, seconds: f32) -> i32;
    fn AXUIElementGetPid(element: AxElement, pid: *mut i32) -> i32;
    fn AXUIElementGetTypeID() -> usize;
}

/// Longest label, identifier or file name kept.
const MAX_CHARS: usize = 60;

/// Each Accessibility message to an app gives up after this.
#[cfg(target_os = "macos")]
const MESSAGE_TIMEOUT_S: f32 = 0.1;

/// A lookup stops reading more attributes after this, whatever it has by then.
const LOOKUP_BUDGET: Duration = Duration::from_millis(250);

/// How long finishing a capture waits for lookups still running.
const DRAIN_WAIT: Duration = Duration::from_millis(400);

/// The element under a point.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hit {
    /// Human role, as the app describes it (`button`, `text field`, `tab`).
    pub role: String,
    pub label: Option<String>,
    pub identifier: Option<String>,
    /// The file name of the document the element's window shows (`AXDocument`).
    pub document: Option<String>,
}

impl Hit {
    /// `“New Tab” #tab-new in main.rs`: what `Click.label` holds (the role goes in `Click.role`).
    pub fn label_text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(label) = &self.label {
            parts.push(format!("“{label}”"));
        }
        if let Some(id) = &self.identifier {
            parts.push(format!("#{id}"));
        }
        if let Some(doc) = &self.document {
            parts.push(format!("in {doc}"));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// `button “Area” #area`: what `Stroke.on` holds. `None` when the element has nothing to name
    /// it by but its role.
    pub fn on_text(&self) -> Option<String> {
        self.label_text()
            .map(|text| format!("{} {text}", self.role))
    }
}

/// One line, at most `MAX_CHARS`, URLs in it without their query (`privacy::clip`).
pub fn clip(raw: &str) -> Option<String> {
    crate::privacy::clip(raw, MAX_CHARS)
}

/// How many ancestors a lookup follows to reach the element's window.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
const MAX_ANCESTORS: usize = 64;

/// Roles that are usually a piece of a control (its text or icon), so the control names it.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn is_part(role: &str) -> bool {
    matches!(role, "AXStaticText" | "AXImage" | "AXGroup" | "AXUnknown")
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn is_control(role: &str) -> bool {
    matches!(
        role,
        "AXButton"
            | "AXLink"
            | "AXMenuItem"
            | "AXMenuBarItem"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXTab"
            | "AXPopUpButton"
            | "AXMenuButton"
            | "AXDisclosureTriangle"
            | "AXCell"
            | "AXRow"
            | "AXSlider"
    )
}

/// Owns a CoreFoundation value returned under the create rule.
#[cfg(target_os = "macos")]
pub(crate) struct Owned(pub(crate) CFTypeRef);

#[cfg(target_os = "macos")]
impl Clone for Owned {
    fn clone(&self) -> Self {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRetain(self.0) };
        }
        Self(self.0)
    }
}

#[cfg(target_os = "macos")]
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRelease(self.0) };
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn attribute(element: AxElement, name: &'static str) -> Option<Owned> {
    let key = CFString::from_static_string(name);
    let mut out: CFTypeRef = std::ptr::null();
    let status =
        unsafe { AXUIElementCopyAttributeValue(element, key.as_concrete_TypeRef(), &raw mut out) };
    (status == 0 && !out.is_null()).then_some(Owned(out))
}

#[cfg(target_os = "macos")]
fn string_attribute(element: AxElement, name: &'static str) -> Option<String> {
    let value = attribute(element, name)?;
    let value = unsafe { CFType::wrap_under_get_rule(value.0) };
    value
        .downcast::<CFString>()
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn element_attribute(element: AxElement, name: &'static str) -> Option<Owned> {
    let value = attribute(element, name)?;
    let is_element =
        unsafe { core_foundation::base::CFGetTypeID(value.0) } == unsafe { AXUIElementGetTypeID() };
    is_element.then_some(value)
}

/// The real Accessibility tree. Every element it hands out gives up on a message after
/// `timeout_s`, so an app that hangs costs one timeout per read, never the caller's thread.
#[cfg(target_os = "macos")]
pub(crate) struct MacTree {
    pub timeout_s: f32,
}

#[cfg(target_os = "macos")]
impl MacTree {
    /// `element` with this tree's timeout set, ready to be asked.
    pub(crate) fn adopt(&self, element: Owned) -> Owned {
        unsafe { AXUIElementSetMessagingTimeout(element.0, self.timeout_s) };
        element
    }
}

#[cfg(target_os = "macos")]
impl Tree for MacTree {
    type Element = Owned;

    fn string(&self, element: &Owned, attribute: &'static str) -> Option<String> {
        string_attribute(element.0, attribute)
    }

    fn url(&self, element: &Owned, name: &'static str) -> Option<String> {
        use core_foundation::url::CFURL;
        let value = attribute(element.0, name)?;
        let value = unsafe { CFType::wrap_under_get_rule(value.0) };
        if let Some(url) = value.downcast::<CFURL>() {
            return Some(url.get_string().to_string()).filter(|s| !s.trim().is_empty());
        }
        value
            .downcast::<CFString>()
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    fn element(&self, element: &Owned, attribute: &'static str) -> Option<Owned> {
        element_attribute(element.0, attribute).map(|found| self.adopt(found))
    }

    fn children(&self, element: &Owned) -> Vec<Owned> {
        self.elements(element, "AXChildren")
    }

    fn key(&self, element: &Owned) -> u64 {
        unsafe { core_foundation::base::CFHash(element.0) as u64 }
    }
}

#[cfg(target_os = "macos")]
impl MacTree {
    /// The elements of a list attribute (`AXChildren`, `AXWindows`).
    pub(crate) fn elements(&self, element: &Owned, name: &'static str) -> Vec<Owned> {
        use core_foundation::array::{CFArray, CFArrayRef};
        let Some(value) = attribute(element.0, name) else {
            return Vec::new();
        };
        if unsafe { core_foundation::base::CFGetTypeID(value.0) } != CFArray::<CFType>::type_id() {
            return Vec::new();
        }
        let array: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(value.0 as CFArrayRef) };
        let element_type = unsafe { AXUIElementGetTypeID() };
        array
            .iter()
            .filter(|child| child.type_of() == element_type)
            .map(|child| {
                let child = child.as_CFTypeRef();
                unsafe { core_foundation::base::CFRetain(child) };
                self.adopt(Owned(child))
            })
            .collect()
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn pid_of(element: AxElement) -> Option<i32> {
    let mut pid = 0;
    (unsafe { AXUIElementGetPid(element, &raw mut pid) } == 0).then_some(pid)
}

#[cfg(target_os = "macos")]
fn element_at(root: AxElement, x: f64, y: f64) -> Option<Owned> {
    let mut out: AxElement = std::ptr::null();
    let status =
        unsafe { AXUIElementCopyElementAtPosition(root, x as f32, y as f32, &raw mut out) };
    (status == 0 && !out.is_null()).then_some(Owned(out))
}

/// The topmost element at a global top-left point, looking past Scrawl's own windows. Blocking;
/// call it off the main thread.
#[cfg(target_os = "macos")]
pub fn hit_test(x: f64, y: f64) -> Option<Hit> {
    let started = Instant::now();
    let own = std::process::id() as i32;
    let system = Owned(unsafe { AXUIElementCreateSystemWide() });
    unsafe { AXUIElementSetMessagingTimeout(system.0, MESSAGE_TIMEOUT_S) };
    let mut element = element_at(system.0, x, y)?;
    if pid_of(element.0) == Some(own) {
        let pid = objc2::rc::autoreleasepool(|_| crate::windows::raw_windows())
            .into_iter()
            .find(|w| {
                let [wx, wy, ww, wh] = w.bounds;
                w.pid != own && w.alpha > 0.01 && x >= wx && x <= wx + ww && y >= wy && y <= wy + wh
            })?
            .pid;
        let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
        unsafe { AXUIElementSetMessagingTimeout(app.0, MESSAGE_TIMEOUT_S) };
        element = element_at(app.0, x, y)?;
    }
    let tree = MacTree {
        timeout_s: MESSAGE_TIMEOUT_S,
    };
    let bundle_id = pid_of(element.0).and_then(|pid| {
        objc2::rc::autoreleasepool(|_| crate::windows::app_meta_of(pid).bundle_id)
    });
    lookup(
        &tree,
        tree.adopt(element),
        bundle_id.as_deref(),
        &|| started.elapsed() > LOOKUP_BUDGET,
    )
}

/// What `element` is, as it may be kept (`crate::privacy`). `bundle_id` is its app's. `over`
/// says the lookup's time is up: it then returns what it has.
///
/// It first reads the roles from the element up to its window. An input on that path is what
/// the point is on, whatever text sits inside it, and its value is never asked for. When the
/// path can't be read to the window in time, or the app is a password manager, only the role
/// is kept.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn lookup<T: Tree>(
    tree: &T,
    element: T::Element,
    bundle_id: Option<&str>,
    over: &dyn Fn() -> bool,
) -> Option<Hit> {
    let mut chain: Vec<(T::Element, String, String)> = Vec::new();
    let mut complete = false;
    let mut next = Some(element);
    while let Some(at) = next.take() {
        if chain.len() >= MAX_ANCESTORS || (!chain.is_empty() && over()) {
            break;
        }
        let role = tree.string(&at, "AXRole").unwrap_or_default();
        let subrole = tree.string(&at, "AXSubrole").unwrap_or_default();
        let top = matches!(role.as_str(), "AXWindow" | "AXApplication");
        if !top {
            next = tree.element(&at, "AXParent");
        }
        chain.push((at, role, subrole));
        if top {
            complete = true;
        }
    }
    let role_only = |role: &str| Hit {
        role: crate::appshot::role_name(role),
        ..Hit::default()
    };
    let (_, first_role, _) = chain.first()?;
    if crate::privacy::is_sensitive_app(bundle_id) || !complete {
        return Some(role_only(first_role));
    }
    let roles: Vec<(String, String)> = chain
        .iter()
        .map(|(_, role, subrole)| (role.clone(), subrole.clone()))
        .collect();
    let index = match crate::privacy::input_over(&roles) {
        Some(input) => input,
        // A button's text or icon names the button.
        None if is_part(first_role) => (1..chain.len().min(3))
            .find(|&i| is_control(&chain[i].1))
            .unwrap_or(0),
        None => 0,
    };
    let (element, role, subrole) = &chain[index];
    let Some((node, _)) = crate::privacy::read(tree, element, role, subrole, over) else {
        return Some(role_only(role));
    };
    let mut hit = Hit {
        role: tree
            .string(element, "AXRoleDescription")
            .and_then(|s| clip(&s))
            .unwrap_or_else(|| crate::appshot::role_name(role)),
        ..Hit::default()
    };
    hit.label = node
        .title
        .or(node.description)
        .or(node.value)
        .or_else(|| (!over()).then(|| tree.string(element, "AXPlaceholderValue")).flatten())
        .and_then(|s| clip(&s));
    if !over() {
        hit.identifier = node
            .identifier
            .or_else(|| tree.string(element, "AXDOMIdentifier"))
            .and_then(|s| clip(&s));
    }
    if !over() {
        let window = match chain.last() {
            Some((window, role, _)) if role == "AXWindow" => Some(window.clone()),
            _ => tree.element(element, "AXWindow"),
        };
        hit.document = window
            .and_then(|window| tree.url(&window, "AXDocument"))
            .and_then(|url| document_name(&url))
            .and_then(|s| clip(&s));
    }
    Some(hit)
}

/// The element at a global point (physical pixels) from UI Automation, looking past Scrawl's own
/// windows. Blocking; call it off the main thread. UI Automation never reads a control's value,
/// and its strings come through `clip` (`crate::privacy`).
#[cfg(target_os = "windows")]
pub fn hit_test(x: f64, y: f64) -> Option<Hit> {
    crate::win::uia::hit_test(x, y, LOOKUP_BUDGET)
}

/// `file:///Users/kit/src/main.rs` → `main.rs`. A URL's query and fragment are dropped first
/// (`page.html?token=…` → `page.html`).
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn document_name(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let path = url.strip_prefix("file://").unwrap_or(url);
    let name = path.trim_end_matches('/').rsplit('/').next()?;
    let decoded = percent_decode(name);
    (!decoded.is_empty()).then_some(decoded)
}

pub(crate) fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a lookup was for, so its result can be put on the right click or stroke at finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// The click or drag that began at this capture time.
    Pointer(u64),
    /// The stroke that began at this capture time.
    Stroke(u64),
}

type Job = (u64, Target, f64, f64);

/// Runs lookups for one capture at a time on a background thread and keeps their results until
/// the capture finishes.
pub struct Prober {
    jobs: flume::Sender<Job>,
    session: Arc<AtomicU64>,
    pending: Arc<AtomicUsize>,
    hits: Arc<Mutex<HashMap<Target, Hit>>>,
}

impl Prober {
    pub fn shared() -> &'static Prober {
        static PROBER: OnceLock<Prober> = OnceLock::new();
        PROBER.get_or_init(|| Prober::start(hit_test))
    }

    fn start(lookup: fn(f64, f64) -> Option<Hit>) -> Self {
        let (jobs, queue) = flume::unbounded::<Job>();
        let session = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(AtomicUsize::new(0));
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let (worker_session, worker_pending, worker_hits) = (
            Arc::clone(&session),
            Arc::clone(&pending),
            Arc::clone(&hits),
        );
        let _ = std::thread::Builder::new()
            .name("scrawl-ax".into())
            .spawn(move || {
                for (job_session, target, x, y) in queue {
                    if job_session == worker_session.load(Ordering::Acquire)
                        && let Some(hit) = lookup(x, y)
                    {
                        worker_hits
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(target, hit);
                    }
                    worker_pending.fetch_sub(1, Ordering::AcqRel);
                }
            });
        Self {
            jobs,
            session,
            pending,
            hits,
        }
    }

    /// Forgets the last capture's results; lookups still queued for it are skipped.
    pub fn begin(&self) {
        self.session.fetch_add(1, Ordering::AcqRel);
        self.hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Looks up the element at a global top-left point for `target`, in the background.
    pub fn probe(&self, target: Target, x: f64, y: f64) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self
            .jobs
            .send((self.session.load(Ordering::Acquire), target, x, y))
            .is_err()
        {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Waits briefly for running lookups, then takes this capture's results.
    pub fn finish(&self) -> HashMap<Target, Hit> {
        let deadline = Instant::now() + DRAIN_WAIT;
        while self.pending.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::mem::take(
            &mut *self
                .hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Puts lookup results on the clicks, drags and strokes they were for.
pub fn apply(
    hits: &HashMap<Target, Hit>,
    events: &mut [crate::model::InteractionEvent],
    strokes: &mut [crate::model::Stroke],
) {
    use crate::model::InteractionEvent;
    for event in events.iter_mut() {
        match event {
            InteractionEvent::Click { t, label, role, .. } => {
                if let Some(hit) = hits.get(&Target::Pointer(*t)) {
                    *role = Some(hit.role.clone());
                    *label = hit.label_text().or(label.take());
                }
            }
            InteractionEvent::Drag { t, label, .. } => {
                if let Some(text) = hits.get(&Target::Pointer(*t)).and_then(Hit::on_text) {
                    *label = Some(text);
                }
            }
            _ => {}
        }
    }
    for stroke in strokes.iter_mut().filter(|s| !s.laser && s.on.is_none()) {
        if let Some(text) = hits.get(&Target::Stroke(stroke.t0)).and_then(Hit::on_text) {
            stroke.on = Some(text);
        }
    }
}

/// Where to look for what a stroke marks: the middle of its box, or just above a flat stroke
/// (an underline sits under the thing it marks). Global top-left points.
pub fn stroke_probe_point(
    stroke: &crate::model::Stroke,
    frame: crate::model::TargetFrame,
) -> Option<(f64, f64)> {
    let points = stroke.normalized_points();
    if points.is_empty() {
        return None;
    }
    let [x0, y0, x1, y1] = crate::sketch::bounds(&points);
    let (w, h) = (
        f64::from(x1 - x0) * frame.width,
        f64::from(y1 - y0) * frame.height,
    );
    let cx = frame.x + f64::from(x0 + x1) / 2.0 * frame.width;
    let top = frame.y + f64::from(y0) * frame.height;
    if h < 24.0 && w > 3.0 * h {
        Some((cx, top - 8.0))
    } else {
        Some((cx, top + h / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::privacy::fake::FakeTree;

    #[test]
    fn labels_are_one_short_line_and_name_the_element() {
        assert_eq!(clip("  New\n  Tab "), Some("New Tab".into()));
        assert_eq!(clip("   "), None);
        let long = "x".repeat(200);
        let clipped = clip(&long).unwrap();
        assert_eq!(clipped.chars().count(), MAX_CHARS);
        assert!(clipped.ends_with('…'));

        let hit = Hit {
            role: "button".into(),
            label: Some("New Tab".into()),
            identifier: Some("tab-new".into()),
            document: Some("main.rs".into()),
        };
        assert_eq!(
            hit.label_text().as_deref(),
            Some("“New Tab” #tab-new in main.rs")
        );
        assert_eq!(
            hit.on_text().as_deref(),
            Some("button “New Tab” #tab-new in main.rs")
        );
        let bare = Hit {
            role: "group".into(),
            ..Hit::default()
        };
        assert_eq!(bare.label_text(), None);
        assert_eq!(bare.on_text(), None, "a role alone names nothing");
        assert_eq!(
            document_name("file:///Users/k/My%20Notes.md").as_deref(),
            Some("My Notes.md")
        );
    }

    const MARKER: &str = "SECRET-MARKER";

    /// A browser window on a page whose URL, title and field all hold a marker:
    /// window › web area › (button › its text, text field › its typed text, a paragraph).
    struct Page {
        tree: FakeTree,
        button_text: usize,
        field: usize,
        typed: usize,
        paragraph: usize,
    }

    fn page() -> Page {
        let mut tree = FakeTree::default();
        let window = tree.add(
            None,
            &[
                ("AXRole", "AXWindow"),
                ("AXTitle", "Fixture file:///f/appshot-secret.html?token=SECRET-MARKER-URL"),
                ("AXDocument", "file:///f/appshot-secret.html?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG"),
            ],
        );
        let web = tree.add(Some(window), &[("AXRole", "AXWebArea")]);
        let button = tree.add(
            Some(web),
            &[
                ("AXRole", "AXButton"),
                ("AXRoleDescription", "button"),
                ("AXTitle", "Save"),
                ("AXDOMIdentifier", "save"),
            ],
        );
        let button_text = tree.add(
            Some(button),
            &[("AXRole", "AXStaticText"), ("AXValue", "Save")],
        );
        let field = tree.add(
            Some(web),
            &[
                ("AXRole", "AXTextField"),
                ("AXRoleDescription", "text field"),
                ("AXDescription", "Name"),
                ("AXValue", "SECRET-MARKER-INPUT"),
            ],
        );
        let group = tree.add(Some(field), &[("AXRole", "AXGroup")]);
        let typed = tree.add(
            Some(group),
            &[
                ("AXRole", "AXStaticText"),
                ("AXTitle", "SECRET-MARKER-INPUT"),
                ("AXValue", "SECRET-MARKER-INPUT"),
            ],
        );
        let paragraph = tree.add(
            Some(web),
            &[
                ("AXRole", "AXStaticText"),
                ("AXValue", "Reset at https://a.dev/reset?token=SECRET-MARKER-URL today"),
            ],
        );
        Page {
            tree,
            button_text,
            field,
            typed,
            paragraph,
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn privacy_a_lookup_names_the_input_and_never_reads_what_was_typed() {
        let page = page();
        for element in [page.typed, page.field] {
            let hit = lookup(&page.tree, element, Some("com.apple.Safari"), &never).unwrap();
            assert_eq!(hit.role, "text field");
            assert_eq!(hit.label.as_deref(), Some("Name"));
            assert!(!format!("{hit:?}").contains(MARKER), "{hit:?}");
        }
        assert!(!page.tree.was_read(page.field, "AXValue"));
        for attribute in ["AXValue", "AXTitle", "AXDescription", "AXPlaceholderValue"] {
            assert!(
                !page.tree.was_read(page.typed, attribute),
                "{attribute} of the text inside a field is that field's value"
            );
        }
    }

    #[test]
    fn privacy_a_lookup_keeps_no_url_query_in_a_label_or_a_document() {
        let page = page();
        let hit = lookup(&page.tree, page.paragraph, None, &never).unwrap();
        assert_eq!(
            hit.label.as_deref(),
            Some("Reset at https://a.dev/reset today")
        );
        assert_eq!(hit.document.as_deref(), Some("appshot-secret.html"));
        let hit = lookup(&page.tree, page.button_text, None, &never).unwrap();
        assert_eq!(
            hit.on_text().as_deref(),
            Some("button “Save” #save in appshot-secret.html"),
            "a button's text names the button"
        );
        assert!(!format!("{hit:?}").contains(MARKER));
        assert_eq!(
            document_name("https://a.dev/x/page.html?token=SECRET-MARKER-URL#frag").as_deref(),
            Some("page.html")
        );
    }

    #[test]
    fn privacy_a_lookup_in_a_password_manager_keeps_only_the_role() {
        let page = page();
        for id in crate::privacy::SENSITIVE_APPS {
            let hit = lookup(&page.tree, page.paragraph, Some(id), &never).unwrap();
            assert_eq!(
                hit,
                Hit {
                    role: "static text".into(),
                    ..Hit::default()
                }
            );
        }
        assert!(!page.tree.was_read(page.paragraph, "AXValue"));
    }

    #[test]
    fn privacy_a_lookup_that_cannot_reach_the_window_keeps_only_the_role() {
        // The text's ancestors can't be read in time, so an input may be among them.
        let page = page();
        let calls = std::cell::Cell::new(0);
        let over = || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        };
        let hit = lookup(&page.tree, page.typed, None, &over).unwrap();
        assert_eq!(hit.label, None);
        assert!(!format!("{hit:?}").contains(MARKER));
        assert!(!page.tree.was_read(page.typed, "AXValue"));
        // An element with no window above it is the same.
        let mut tree = FakeTree::default();
        let orphan = tree.add(
            None,
            &[("AXRole", "AXStaticText"), ("AXValue", "SECRET-MARKER-INPUT")],
        );
        let hit = lookup(&tree, orphan, None, &never).unwrap();
        assert_eq!(hit.label, None);
        assert!(!tree.was_read(orphan, "AXValue"));
    }

    #[test]
    fn privacy_a_click_and_a_mark_on_typed_text_store_no_typed_text() {
        let page = page();
        let mut hits = HashMap::new();
        for target in [Target::Pointer(100), Target::Stroke(500)] {
            hits.insert(
                target,
                lookup(&page.tree, page.typed, None, &never).unwrap(),
            );
        }
        let mut events = vec![InteractionEvent::Click {
            t: 100,
            x: 0.1,
            y: 0.1,
            button: None,
            label: None,
            role: None,
        }];
        let mut strokes = vec![Stroke {
            t0: 500,
            t1: 510,
            fw: 100,
            fh: 100,
            on: None,
            pin: None,
            laser: false,
            pts: vec![0, 0, 0],
        }];
        apply(&hits, &mut events, &mut strokes);
        let stored = format!(
            "{} {}",
            serde_json::to_string(&events).unwrap(),
            serde_json::to_string(&strokes).unwrap()
        );
        assert!(!stored.contains(MARKER), "{stored}");
        assert!(stored.contains("text field"), "{stored}");
        assert_eq!(strokes[0].on.as_deref(), Some("text field “Name” in appshot-secret.html"));
    }

    #[test]
    fn results_land_on_the_click_or_stroke_they_were_for() {
        let mut hits = HashMap::new();
        hits.insert(
            Target::Pointer(100),
            Hit {
                role: "button".into(),
                label: Some("Area".into()),
                ..Hit::default()
            },
        );
        hits.insert(
            Target::Stroke(500),
            Hit {
                role: "tab".into(),
                label: Some("Window".into()),
                ..Hit::default()
            },
        );
        let mut events = vec![
            InteractionEvent::Click {
                t: 100,
                x: 0.1,
                y: 0.1,
                button: None,
                label: None,
                role: None,
            },
            InteractionEvent::Click {
                t: 200,
                x: 0.1,
                y: 0.1,
                button: None,
                label: None,
                role: None,
            },
        ];
        let stroke = |t0| Stroke {
            t0,
            t1: t0 + 10,
            fw: 100,
            fh: 100,
            on: None,
            pin: None,
            laser: false,
            pts: vec![0, 0, 0],
        };
        let mut strokes = vec![stroke(500), stroke(600)];
        apply(&hits, &mut events, &mut strokes);
        assert!(
            matches!(&events[0], InteractionEvent::Click { label: Some(l), role: Some(r), .. } if l == "“Area”" && r == "button")
        );
        assert!(matches!(
            &events[1],
            InteractionEvent::Click {
                label: None,
                role: None,
                ..
            }
        ));
        assert_eq!(strokes[0].on.as_deref(), Some("tab “Window”"));
        assert_eq!(strokes[1].on, None);
    }

    #[test]
    fn the_prober_keeps_only_this_captures_results() {
        fn fake(x: f64, _y: f64) -> Option<Hit> {
            (x > 0.0).then(|| Hit {
                role: "button".into(),
                label: Some(format!("{x}")),
                ..Hit::default()
            })
        }
        let prober = Prober::start(fake);
        prober.begin();
        prober.probe(Target::Pointer(1), 5.0, 0.0);
        prober.probe(Target::Pointer(2), -1.0, 0.0);
        let hits = prober.finish();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[&Target::Pointer(1)].label.as_deref(), Some("5"));
        prober.begin();
        assert!(prober.finish().is_empty());
    }

    #[test]
    fn underlines_are_probed_just_above() {
        let frame = TargetFrame {
            x: 100.0,
            y: 50.0,
            width: 1000.0,
            height: 1000.0,
            scale: 2.0,
        };
        let flat = Stroke {
            t0: 0,
            t1: 10,
            fw: 1000,
            fh: 1000,
            on: None,
            pin: None,
            laser: false,
            pts: vec![1000, 5000, 0, 3000, 5050, 5],
        };
        let (x, y) = stroke_probe_point(&flat, frame).unwrap();
        assert!(
            (x - 300.0).abs() < 0.5 && (y - 542.0).abs() < 0.5,
            "{x} {y}"
        );
        let circle = Stroke {
            pts: vec![1000, 1000, 0, 3000, 3000, 5],
            ..flat
        };
        let (x, y) = stroke_probe_point(&circle, frame).unwrap();
        assert!(
            (x - 300.0).abs() < 0.5 && (y - 250.0).abs() < 0.5,
            "{x} {y}"
        );
    }
}
