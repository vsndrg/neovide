//! Native WKWebViews overlaid on Nvim windows.
//!
//! Nvim (through `neovide.webview.*`) asks for a webview bound to a window handle (`winid`).
//! Every frame the webview is moved onto that window's pixel rect, hidden when the window is not
//! displayed, and masked where floating windows cover it, so the Nvim UI stays on top.
//! Page script talks back through `window.webkit.messageHandlers.neovide.postMessage(string)`,
//! which is forwarded to `neovide.private.webview_message(id, string)` in Nvim.
//!
//! Keyboard: while a webview is first responder, only the keys Nvim declared for it (plus
//! Cmd+C / Cmd+A for selections) reach the page. Every other key event is handed to the editor
//! view synchronously, so it enters Nvim's input in order with the keys typed after it.
//!
//! Trackpad: the events of a scroll gesture reach WebKit resampled to the display, one per frame
//! (see `wheel_resampler`).

use std::{cell::RefCell, collections::HashMap};

use glamour::Intersection;

use objc2::{
    DefinedClass, MainThreadOnly, Message, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::ProtocolObject,
    sel,
};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSEventPhase, NSResponder, NSScreen, NSView, NSWindow,
};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventField, CGMutablePath};
use objc2_foundation::{
    MainThreadMarker, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSURL, ns_string,
};
use objc2_quartz_core::{CADisplayLink, CAShapeLayer, CATransaction};

use super::wheel_resampler::WheelResampler;
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};

use crate::bridge::{NeovimHandler, ParallelCommand, SerialCommand, send_ui};
use crate::units::PixelRect;

const MESSAGE_HANDLER_NAME: &str = "neovide";

#[derive(Debug)]
struct MessageHandlerIvars {
    id: u64,
    neovim_handler: NeovimHandler,
    ns_window: Weak<NSWindow>,
}

define_class!(
    #[derive(Debug)]
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = MessageHandlerIvars]
    struct WebviewMessageHandler;

    unsafe impl NSObjectProtocol for WebviewMessageHandler {}

    unsafe impl WKScriptMessageHandler for WebviewMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_script_message(
            &self,
            _controller: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Ok(body) = body.downcast::<NSString>() else {
                log::warn!("webview: ignoring non-string message");
                return;
            };
            let body = body.to_string();
            let ivars = self.ivars();
            // `{"type":"blur","key":"<keys>"}` hands the keyboard back to Nvim and replays the key
            // the page did not handle. Both happen here rather than in Nvim: focus must move
            // before the next key press arrives, and the replayed key must enter the same ordered
            // input queue as the keys typed after it.
            if let Some(key) = parse_blur(&body) {
                if let Some(ns_window) = ivars.ns_window.load() {
                    focus_nvim(&ns_window);
                }
                if let Some(key) = key {
                    send_ui(SerialCommand::Keyboard(key), &ivars.neovim_handler);
                }
            }
            send_ui(
                ParallelCommand::WebviewMessage { id: ivars.id, message: body },
                &ivars.neovim_handler,
            );
        }
    }
);

impl WebviewMessageHandler {
    fn new(
        mtm: MainThreadMarker,
        id: u64,
        neovim_handler: NeovimHandler,
        ns_window: &NSWindow,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MessageHandlerIvars {
            id,
            neovim_handler,
            ns_window: Weak::from(ns_window),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// `Some(key)` for a blur message (`key` is the Nvim key notation to replay, if any).
fn parse_blur(body: &str) -> Option<Option<String>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value.get("type")?.as_str()? != "blur" {
        return None;
    }
    Some(value.get("key").and_then(|key| key.as_str()).map(str::to_owned))
}

fn focus_nvim(ns_window: &NSWindow) {
    if let Some(content_view) = ns_window.contentView() {
        ns_window.makeFirstResponder(Some(&content_view));
    }
}

/// A key the page receives: a macOS virtual key code plus exact modifiers. Letters are matched by
/// physical key (ANSI positions), so they work in any keyboard layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeySpec {
    code: u16,
    modifiers: NSEventModifierFlags,
}

const RELEVANT_MODIFIERS: NSEventModifierFlags = NSEventModifierFlags::Shift
    .union(NSEventModifierFlags::Control)
    .union(NSEventModifierFlags::Option)
    .union(NSEventModifierFlags::Command);

const LETTER_KEY_CODES: [u16; 26] =
    [0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6];

impl KeySpec {
    /// Parses Nvim key notation: `j`, `G`, `<Esc>`, `<PageDown>`, `<D-c>`, `<S-Down>`...
    fn parse(spec: &str) -> Option<Self> {
        let (mods, key) = match spec.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            Some(inner) => {
                let mut parts: Vec<&str> = inner.split('-').collect();
                let key = parts.pop()?;
                let mut mods = NSEventModifierFlags::empty();
                for part in parts {
                    mods |= match part {
                        "S" => NSEventModifierFlags::Shift,
                        "C" => NSEventModifierFlags::Control,
                        "M" | "A" => NSEventModifierFlags::Option,
                        "D" => NSEventModifierFlags::Command,
                        _ => return None,
                    };
                }
                (mods, key)
            }
            None => (NSEventModifierFlags::empty(), spec),
        };
        let mut chars = key.chars();
        let (code, shift) = match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => (
                LETTER_KEY_CODES[(c.to_ascii_lowercase() as u8 - b'a') as usize],
                c.is_ascii_uppercase(),
            ),
            _ => (
                match key {
                    "Esc" => 53,
                    "CR" => 36,
                    "Tab" => 48,
                    "BS" => 51,
                    "Space" => 49,
                    "Up" => 126,
                    "Down" => 125,
                    "Left" => 123,
                    "Right" => 124,
                    "PageUp" => 116,
                    "PageDown" => 121,
                    "Home" => 115,
                    "End" => 119,
                    _ => return None,
                },
                false,
            ),
        };
        let modifiers = if shift { mods | NSEventModifierFlags::Shift } else { mods };
        Some(Self { code, modifiers })
    }

    fn matches(&self, event: &NSEvent) -> bool {
        event.keyCode() == self.code
            && (event.modifierFlags() & RELEVANT_MODIFIERS) == self.modifiers
    }
}

/// Selection editing works in any focused webview.
fn selection_keys() -> [KeySpec; 2] {
    [KeySpec::parse("<D-c>").unwrap(), KeySpec::parse("<D-a>").unwrap()]
}

#[derive(Debug)]
struct KeyRoutingIvars {
    id: u64,
    neovim_handler: NeovimHandler,
    page_keys: RefCell<Vec<KeySpec>>,
    wheel: RefCell<WheelGesture>,
}

/// The trackpad gesture being resampled.
#[derive(Debug, Default)]
struct WheelGesture {
    resampler: WheelResampler,
    /// Latest event of the gesture: what resampled events are copied from.
    template: Option<Retained<NSEvent>>,
    /// Ticks while a gesture is resampled (it retains the view, so it is invalidated after).
    link: Option<Retained<CADisplayLink>>,
}

/// `kCGScrollWheelEventScrollPhase` value of a gesture's ongoing events.
const CG_SCROLL_PHASE_CHANGED: i64 = 2;

define_class!(
    #[derive(Debug)]
    #[unsafe(super(WKWebView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = KeyRoutingIvars]
    struct KeyRoutingWebView;

    impl KeyRoutingWebView {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if self.routes_to_page(event) {
                unsafe { msg_send![super(self), keyDown: event] }
            } else if let Some(editor) = self.give_focus_to_editor() {
                unsafe { msg_send![&editor, keyDown: event] }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if self.routes_to_page(event) {
                unsafe { msg_send![super(self), keyUp: event] }
            } else if let Some(editor) = self.window().and_then(|window| window.contentView()) {
                unsafe { msg_send![&editor, keyUp: event] }
            }
        }

        // Nvim tracks whether the page has the keyboard; tell it whenever that ends, whatever
        // the cause (a key routed to Nvim, a click on the editor, focus(false)).
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let resigned: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if resigned {
                self.set_page_keys(Vec::new());
                send_ui(
                    ParallelCommand::WebviewMessage {
                        id: self.ivars().id,
                        message: r#"{"type":"blur"}"#.to_owned(),
                    },
                    &self.ivars().neovim_handler,
                );
            }
            resigned
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.on_scroll_wheel(event);
        }

        #[unsafe(method(wheelFrame:))]
        fn wheel_frame(&self, link: &CADisplayLink) {
            self.on_wheel_frame(link);
        }

        // Cmd shortcuts go through key equivalents first: keep the ones the page does not own
        // away from WebKit so they reach Nvim through keyDown: above.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            if self.is_first_responder() && self.routes_to_page(event) {
                unsafe { msg_send![super(self), performKeyEquivalent: event] }
            } else {
                false
            }
        }
    }
);

impl KeyRoutingWebView {
    fn new(
        mtm: MainThreadMarker,
        configuration: &WKWebViewConfiguration,
        id: u64,
        neovim_handler: NeovimHandler,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(KeyRoutingIvars {
            id,
            neovim_handler,
            page_keys: RefCell::new(Vec::new()),
            wheel: RefCell::new(WheelGesture::default()),
        });
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO, configuration: configuration] }
    }

    fn routes_to_page(&self, event: &NSEvent) -> bool {
        selection_keys().iter().any(|key| key.matches(event))
            || self.ivars().page_keys.borrow().iter().any(|key| key.matches(event))
    }

    fn set_page_keys(&self, keys: Vec<KeySpec>) {
        *self.ivars().page_keys.borrow_mut() = keys;
    }

    fn is_first_responder(&self) -> bool {
        self.window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| is_descendant_responder(&responder, self))
    }

    // A trackpad gesture's own events (phase began/changed/ended) are resampled; mouse wheels,
    // the momentum the system generates after a flick (already even) and anything outside a
    // gesture go to WebKit as they come.
    fn on_scroll_wheel(&self, event: &NSEvent) {
        let phase = event.phase();
        let resampled = event.hasPreciseScrollingDeltas()
            && event.momentumPhase().is_empty()
            && phase.intersects(
                NSEventPhase::Began
                    | NSEventPhase::Changed
                    | NSEventPhase::Stationary
                    | NSEventPhase::Ended
                    | NSEventPhase::Cancelled,
            );
        if !resampled {
            self.end_wheel_gesture(None);
            unsafe { msg_send![super(self), scrollWheel: event] }
            return;
        }
        let time = event.timestamp();
        let delta = [event.scrollingDeltaX(), event.scrollingDeltaY()];
        if phase.contains(NSEventPhase::Began) {
            self.end_wheel_gesture(None);
            self.begin_wheel_gesture(time);
            unsafe { msg_send![super(self), scrollWheel: event] }
            return;
        }
        if !self.ivars().wheel.borrow().resampler.is_active() {
            unsafe { msg_send![super(self), scrollWheel: event] }
            return;
        }
        {
            let mut wheel = self.ivars().wheel.borrow_mut();
            wheel.resampler.sample(time, delta);
            wheel.template = Some(event.retain());
        }
        if phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled) {
            // The ending event carries whatever the frames have not handed out yet.
            self.end_wheel_gesture(Some(event));
        }
    }

    fn begin_wheel_gesture(&self, time: f64) {
        let link = unsafe { self.displayLinkWithTarget_selector(self, sel!(wheelFrame:)) };
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        let mut wheel = self.ivars().wheel.borrow_mut();
        wheel.resampler.begin(time);
        wheel.link = Some(link);
    }

    /// Ends the resampled gesture, if any, handing out the rest of its input with `last` (the
    /// gesture's ending event), or with its latest event.
    fn end_wheel_gesture(&self, last: Option<&NSEvent>) {
        let (rest, template) = {
            let mut wheel = self.ivars().wheel.borrow_mut();
            if let Some(link) = wheel.link.take() {
                link.invalidate();
            }
            if !wheel.resampler.is_active() {
                return;
            }
            let rest = wheel.resampler.finish();
            (rest, wheel.template.take())
        };
        match last {
            Some(event) => self.send_wheel(event, rest, None, None),
            None if rest != [0.0; 2] => {
                if let Some(template) = template {
                    self.send_wheel(&template, rest, Some(CG_SCROLL_PHASE_CHANGED), None);
                }
            }
            None => {}
        }
    }

    fn on_wheel_frame(&self, link: &CADisplayLink) {
        let target = link.targetTimestamp();
        let (step, template, idle) = {
            let mut wheel = self.ivars().wheel.borrow_mut();
            if !wheel.resampler.is_active() {
                return;
            }
            let step = wheel.resampler.frame(target, target - link.timestamp());
            (step, wheel.template.clone(), wheel.resampler.is_idle(target))
        };
        if let Some(template) = template
            && step != [0.0; 2]
        {
            self.send_wheel(&template, step, Some(CG_SCROLL_PHASE_CHANGED), Some(target));
        }
        // A gesture whose end never arrived (focus moved away mid-gesture) stops here.
        if idle && self.window().is_none_or(|window| !window.isKeyWindow()) {
            self.end_wheel_gesture(None);
        }
    }

    /// Hands WebKit a copy of `template` carrying `delta` ([x, y], px), optionally with another
    /// gesture phase and timestamp.
    fn send_wheel(
        &self,
        template: &NSEvent,
        delta: [f64; 2],
        phase: Option<i64>,
        time: Option<f64>,
    ) {
        let Some(source) = template.CGEvent() else {
            return;
        };
        let Some(copy) = CGEvent::new_copy(Some(&source)) else {
            return;
        };
        let event = Some(&*copy);
        let axes = [
            (
                delta[1],
                CGEventField::ScrollWheelEventDeltaAxis1,
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
                CGEventField::ScrollWheelEventPointDeltaAxis1,
            ),
            (
                delta[0],
                CGEventField::ScrollWheelEventDeltaAxis2,
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
                CGEventField::ScrollWheelEventPointDeltaAxis2,
            ),
        ];
        for (pixels, line_field, fixed_field, point_field) in axes {
            // Each field is derived from the ones set after it: writing the whole lines last
            // turned the pixel delta into 8 px per line. So lines, fractional lines, then pixels,
            // in the template's lines-per-pixel ratio.
            let template_pixels = CGEvent::double_value_field(event, point_field);
            let ratio = if template_pixels != 0.0 {
                CGEvent::double_value_field(event, fixed_field) / template_pixels
            } else {
                0.1
            };
            let lines = pixels * ratio;
            CGEvent::set_integer_value_field(event, line_field, lines.round() as i64);
            CGEvent::set_double_value_field(event, fixed_field, lines);
            CGEvent::set_double_value_field(event, point_field, pixels);
        }
        if let Some(phase) = phase {
            CGEvent::set_integer_value_field(
                event,
                CGEventField::ScrollWheelEventScrollPhase,
                phase,
            );
        }
        if let Some(time) = time {
            // CGEvent timestamps count in other units than NSEvent's seconds; scale by the
            // template's pair.
            let (cg, ns) = (CGEvent::timestamp(event) as f64, template.timestamp());
            if ns > 0.0 {
                CGEvent::set_timestamp(event, (time * cg / ns) as u64);
            }
        }
        let Some(mut synthesized) = NSEvent::eventWithCGEvent(&copy) else {
            return;
        };
        let mtm = MainThreadMarker::from(self);
        if synthesized.window(mtm).is_none() {
            // Without a window, AppKit reports the screen point as the location in the window.
            // Place it so that it is the template's location in the window.
            let Some(screen) = NSScreen::screens(mtm).firstObject() else {
                return;
            };
            let location = template.locationInWindow();
            CGEvent::set_location(
                event,
                CGPoint::new(location.x, screen.frame().size.height - location.y),
            );
            let Some(placed) = NSEvent::eventWithCGEvent(&copy) else {
                return;
            };
            synthesized = placed;
        }
        unsafe { msg_send![super(self), scrollWheel: &*synthesized] }
    }

    fn stop_wheel(&self) {
        if let Some(link) = self.ivars().wheel.borrow_mut().link.take() {
            link.invalidate();
        }
    }

    fn give_focus_to_editor(&self) -> Option<Retained<NSView>> {
        let window = self.window()?;
        let editor = window.contentView()?;
        window.makeFirstResponder(Some(&editor));
        Some(editor)
    }
}

/// Where a webview should be this frame, in physical pixels relative to the content view.
pub struct WebviewPlacement {
    pub region: PixelRect<f32>,
    /// Regions of floating windows drawn above the target window.
    pub occluders: Vec<PixelRect<f32>>,
}

#[derive(Debug)]
struct Pane {
    view: Retained<KeyRoutingWebView>,
    handler: Retained<WebviewMessageHandler>,
    winid: u64,
    frame: Option<NSRect>,
    holes: Vec<NSRect>,
}

impl Pane {
    fn set_hidden(&mut self, hidden: bool) {
        if self.view.isHidden() != hidden {
            self.view.setHidden(hidden);
        }
        if hidden {
            self.frame = None;
        }
    }

    fn place(&mut self, frame: NSRect, holes: Vec<NSRect>) {
        if self.frame != Some(frame) {
            self.view.setFrame(frame);
            self.frame = Some(frame);
            // The mask is in view coordinates; it depends on the size too.
            self.holes.clear();
            self.holes.push(NSRect::ZERO);
        }
        if self.holes != holes {
            self.update_mask(frame.size, &holes);
            self.holes = holes;
        }
        self.set_hidden(false);
    }

    fn update_mask(&self, size: NSSize, holes: &[NSRect]) {
        let Some(layer) = self.view.layer() else {
            return;
        };
        if holes.is_empty() {
            unsafe { layer.setMask(None) };
            return;
        }
        // Holes are in (flipped, y-down) view coordinates. A layer inside a flipped view
        // hierarchy has flipped geometry (reported by contentsAreFlipped), so the path can use
        // them as is; otherwise convert to the y-up layer space.
        let flip = !layer.contentsAreFlipped();
        let path = CGMutablePath::new();
        for rect in subtract_rects(NSRect::new(NSPoint::ZERO, size), holes) {
            let rect = if flip {
                NSRect::new(
                    NSPoint::new(rect.origin.x, size.height - rect.origin.y - rect.size.height),
                    rect.size,
                )
            } else {
                rect
            };
            unsafe { CGMutablePath::add_rect(Some(&path), std::ptr::null(), rect) };
        }
        let mask = CAShapeLayer::new();
        mask.setFrame(NSRect::new(NSPoint::ZERO, size));
        mask.setPath(Some(&path));
        unsafe { layer.setMask(Some(&mask)) };
    }
}

/// Splits `bounds` minus the union of `holes` into disjoint rectangles.
fn subtract_rects(bounds: NSRect, holes: &[NSRect]) -> Vec<NSRect> {
    let mut pieces = vec![bounds];
    for hole in holes {
        let mut next = Vec::with_capacity(pieces.len() + 4);
        for piece in pieces {
            let (px0, py0) = (piece.origin.x, piece.origin.y);
            let (px1, py1) = (px0 + piece.size.width, py0 + piece.size.height);
            let x0 = hole.origin.x.max(px0);
            let y0 = hole.origin.y.max(py0);
            let x1 = (hole.origin.x + hole.size.width).min(px1);
            let y1 = (hole.origin.y + hole.size.height).min(py1);
            if x0 >= x1 || y0 >= y1 {
                next.push(piece);
                continue;
            }
            let mut push = |x: f64, y: f64, w: f64, h: f64| {
                if w > 0.0 && h > 0.0 {
                    next.push(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
                }
            };
            push(px0, py0, px1 - px0, y0 - py0); // above
            push(px0, y1, px1 - px0, py1 - y1); // below
            push(px0, y0, x0 - px0, y1 - y0); // left
            push(x1, y0, px1 - x1, y1 - y0); // right
        }
        pieces = next;
    }
    pieces
}

#[derive(Debug, Default)]
pub struct WebviewManager {
    panes: HashMap<u64, Pane>,
}

impl WebviewManager {
    pub fn open(
        &mut self,
        ns_window: &NSWindow,
        neovim_handler: &NeovimHandler,
        id: u64,
        winid: u64,
        url: &str,
    ) {
        let mtm = MainThreadMarker::new().expect("webviews must be created on the main thread");
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.winid = winid;
            load_file(&pane.view, url);
            return;
        }
        let Some(content_view) = ns_window.contentView() else {
            return;
        };

        let handler = WebviewMessageHandler::new(mtm, id, neovim_handler.clone(), ns_window);
        let view = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            // WebKit throttles page rendering updates (rAF, scroll commits) to ~60 fps even on
            // 120 Hz displays unless this feature flag is off.
            set_webkit_feature(&configuration, "PreferPageRenderingUpdatesNear60FPSEnabled", false);
            configuration.userContentController().addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(MESSAGE_HANDLER_NAME),
            );
            let view = KeyRoutingWebView::new(mtm, &configuration, id, neovim_handler.clone());
            // Let the window background (drawn by Neovide) show until the page paints.
            let _: () = msg_send![&view, setValue: &*NSNumber::new_bool(false), forKey: ns_string!("drawsBackground")];
            view.setInspectable(true);
            view
        };
        view.setWantsLayer(true);
        view.setHidden(true);
        content_view.addSubview(&view);
        load_file(&view, url);

        self.panes.insert(id, Pane { view, handler, winid, frame: None, holes: Vec::new() });
    }

    pub fn set_window(&mut self, id: u64, winid: u64) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.winid = winid;
        }
    }

    pub fn post(&self, id: u64, message: &str) {
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        // A JSON string literal is a valid JS string literal.
        let literal = serde_json::to_string(message).unwrap_or_default();
        let script = format!("window.neovideReceive && window.neovideReceive({literal})");
        unsafe {
            pane.view.evaluateJavaScript_completionHandler(&NSString::from_str(&script), None);
        }
    }

    /// `keys` (Nvim key notation) are delivered to the page while it has focus; all other keys
    /// return focus to Nvim.
    pub fn focus(&self, ns_window: &NSWindow, id: u64, focus: bool, keys: &[String]) {
        match self.panes.get(&id) {
            Some(pane) if focus => {
                let specs = keys
                    .iter()
                    .filter_map(|key| {
                        let spec = KeySpec::parse(key);
                        if spec.is_none() {
                            log::warn!("webview: unsupported key {key:?}");
                        }
                        spec
                    })
                    .collect();
                pane.view.set_page_keys(specs);
                ns_window.makeFirstResponder(Some(&*pane.view));
            }
            Some(pane) => {
                pane.view.set_page_keys(Vec::new());
                focus_nvim(ns_window);
            }
            None => focus_nvim(ns_window),
        }
    }

    pub fn close(&mut self, ns_window: &NSWindow, id: u64) {
        let Some(pane) = self.panes.remove(&id) else {
            return;
        };
        let had_focus = pane.view.is_first_responder();
        unsafe {
            pane.view
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(MESSAGE_HANDLER_NAME));
        }
        pane.view.stop_wheel();
        pane.view.removeFromSuperview();
        drop(pane.handler);
        if had_focus {
            focus_nvim(ns_window);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Moves every webview onto its window. `placement` resolves a `winid` to its current
    /// region, or `None` when the window is not displayed.
    pub fn sync(&mut self, scale_factor: f64, placement: impl Fn(u64) -> Option<WebviewPlacement>) {
        if self.panes.is_empty() {
            return;
        }
        let to_points = |rect: PixelRect<f32>| {
            let size = rect.size();
            NSRect::new(
                NSPoint::new(rect.min.x as f64 / scale_factor, rect.min.y as f64 / scale_factor),
                NSSize::new(size.width as f64 / scale_factor, size.height as f64 / scale_factor),
            )
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for pane in self.panes.values_mut() {
            let Some(WebviewPlacement { region, occluders }) = placement(pane.winid) else {
                pane.set_hidden(true);
                continue;
            };
            let frame = to_points(region);
            let holes = occluders
                .into_iter()
                .filter(|o| o.intersects(&region))
                .map(|o| {
                    let o = to_points(o);
                    NSRect::new(
                        NSPoint::new(o.origin.x - frame.origin.x, o.origin.y - frame.origin.y),
                        o.size,
                    )
                })
                .collect();
            pane.place(frame, holes);
        }
        CATransaction::commit();
    }
}

/// Toggles a WebKit feature flag (the switches behind Safari's Feature Flags settings) through
/// `+[WKPreferences _features]` / `-[WKPreferences _setEnabled:forFeature:]`. These are SPI;
/// when they are missing the flag is left at its default.
fn set_webkit_feature(configuration: &WKWebViewConfiguration, key: &str, enabled: bool) {
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::sel;
    use objc2_foundation::NSArray;

    let preferences = unsafe { configuration.preferences() };
    let class: &AnyClass = preferences.class();
    let features_sel: Sel = sel!(_features);
    let set_sel: Sel = sel!(_setEnabled:forFeature:);
    // `_features` is a class method: ask the class object, not its instances.
    let has_api: bool = unsafe { msg_send![class, respondsToSelector: features_sel] }
        && unsafe { msg_send![&*preferences, respondsToSelector: set_sel] };
    if !has_api {
        log::warn!("webview: WKPreferences feature SPI unavailable, cannot set {key}");
        return;
    }
    let features: Retained<NSArray<AnyObject>> = unsafe { msg_send![class, _features] };
    for feature in features.iter() {
        let feature_key: Retained<NSString> = unsafe { msg_send![&*feature, key] };
        if feature_key.to_string() == key {
            unsafe {
                let _: () = msg_send![&*preferences, _setEnabled: enabled, forFeature: &*feature];
            }
            return;
        }
    }
    log::warn!("webview: WebKit feature {key} not found");
}

fn load_file(view: &WKWebView, path: &str) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    // Read access to the whole file system: pages reference images next to the documents
    // they render.
    let root = NSURL::fileURLWithPath(ns_string!("/"));
    unsafe {
        let _ = view.loadFileURL_allowingReadAccessToURL(&url, &root);
    }
}

fn is_descendant_responder(responder: &NSResponder, view: &NSView) -> bool {
    responder.downcast_ref::<NSView>().is_some_and(|responder| responder.isDescendantOf(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    fn area(rects: &[NSRect]) -> f64 {
        rects.iter().map(|r| r.size.width * r.size.height).sum()
    }

    #[test]
    fn subtract_no_holes_keeps_bounds() {
        let bounds = rect(0.0, 0.0, 100.0, 50.0);
        assert_eq!(subtract_rects(bounds, &[]), vec![bounds]);
    }

    #[test]
    fn subtract_inner_hole() {
        let pieces = subtract_rects(rect(0.0, 0.0, 100.0, 100.0), &[rect(10.0, 20.0, 30.0, 40.0)]);
        assert_eq!(area(&pieces), 100.0 * 100.0 - 30.0 * 40.0);
        assert_eq!(pieces.len(), 4);
    }

    #[test]
    fn subtract_overlapping_holes_counts_union_once() {
        let pieces = subtract_rects(
            rect(0.0, 0.0, 100.0, 100.0),
            &[rect(0.0, 0.0, 50.0, 50.0), rect(25.0, 25.0, 50.0, 50.0)],
        );
        // Union of the holes: 2 * 2500 - 625 overlap.
        assert_eq!(area(&pieces), 10000.0 - (5000.0 - 625.0));
    }

    #[test]
    fn subtract_hole_outside_is_ignored() {
        let bounds = rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(subtract_rects(bounds, &[rect(20.0, 20.0, 5.0, 5.0)]), vec![bounds]);
    }

    #[test]
    fn subtract_covering_hole_leaves_nothing() {
        assert!(
            subtract_rects(rect(0.0, 0.0, 10.0, 10.0), &[rect(-1.0, -1.0, 20.0, 20.0)]).is_empty()
        );
    }

    #[test]
    fn key_spec_parses_letters_by_physical_key() {
        let j = KeySpec::parse("j").unwrap();
        assert_eq!((j.code, j.modifiers), (38, NSEventModifierFlags::empty()));
        let g = KeySpec::parse("G").unwrap();
        assert_eq!((g.code, g.modifiers), (5, NSEventModifierFlags::Shift));
        let copy = KeySpec::parse("<D-c>").unwrap();
        assert_eq!((copy.code, copy.modifiers), (8, NSEventModifierFlags::Command));
        let down = KeySpec::parse("<S-Down>").unwrap();
        assert_eq!((down.code, down.modifiers), (125, NSEventModifierFlags::Shift));
        assert_eq!(KeySpec::parse("<Esc>").unwrap().code, 53);
        assert!(KeySpec::parse("<X-j>").is_none());
        assert!(KeySpec::parse("<F13>").is_none());
    }

    #[test]
    fn parse_blur_extracts_replay_key() {
        assert_eq!(parse_blur(r#"{"type":"blur","key":"<Space>"}"#), Some(Some("<Space>".into())));
        assert_eq!(parse_blur(r#"{"type":"blur"}"#), Some(None));
        assert_eq!(parse_blur(r#"{"type":"click","line":3}"#), None);
        assert_eq!(parse_blur("not json"), None);
    }
}
