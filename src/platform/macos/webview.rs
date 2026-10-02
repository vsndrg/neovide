//! Native WKWebViews overlaid on Nvim windows.
//!
//! Nvim (through `neovide.webview.*`) asks for a webview bound to a window handle (`winid`).
//! Every frame the webview is moved onto that window's pixel rect, hidden when the window is not
//! displayed, and masked where floating windows cover it, so the Nvim UI stays on top.
//! Page script talks back through `window.webkit.messageHandlers.neovide.postMessage(string)`,
//! which is forwarded to `neovide.private.webview_message(id, string)` in Nvim.

use std::collections::HashMap;

use glamour::Intersection;

use objc2::{
    DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::ProtocolObject,
};
use objc2_app_kit::{NSView, NSWindow};
use objc2_core_graphics::CGMutablePath;
use objc2_foundation::{
    MainThreadMarker, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    NSURL, ns_string,
};
use objc2_quartz_core::{CAShapeLayer, CATransaction};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};

use crate::bridge::{NeovimHandler, ParallelCommand, send_ui};
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
            // Handing keyboard focus back must not wait for an Nvim round trip: the next key
            // press would still land in the webview.
            if message_type(&body).as_deref() == Some("blur")
                && let Some(ns_window) = ivars.ns_window.load()
            {
                focus_nvim(&ns_window);
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

fn message_type(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(value.get("type")?.as_str()?.to_owned())
}

fn focus_nvim(ns_window: &NSWindow) {
    if let Some(content_view) = ns_window.contentView() {
        ns_window.makeFirstResponder(Some(&content_view));
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
    view: Retained<WKWebView>,
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
            configuration.userContentController().addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(MESSAGE_HANDLER_NAME),
            );
            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &configuration,
            );
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

    pub fn focus(&self, ns_window: &NSWindow, id: u64, focus: bool) {
        match self.panes.get(&id) {
            Some(pane) if focus => {
                ns_window.makeFirstResponder(Some(&pane.view));
            }
            _ => focus_nvim(ns_window),
        }
    }

    pub fn close(&mut self, ns_window: &NSWindow, id: u64) {
        let Some(pane) = self.panes.remove(&id) else {
            return;
        };
        let had_focus = ns_window
            .firstResponder()
            .is_some_and(|responder| is_descendant_responder(&responder, &pane.view));
        unsafe {
            pane.view
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(MESSAGE_HANDLER_NAME));
        }
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

fn load_file(view: &WKWebView, path: &str) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    // Read access to the whole file system: pages reference images next to the documents
    // they render.
    let root = NSURL::fileURLWithPath(ns_string!("/"));
    unsafe {
        let _ = view.loadFileURL_allowingReadAccessToURL(&url, &root);
    }
}

fn is_descendant_responder(responder: &objc2_app_kit::NSResponder, view: &NSView) -> bool {
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
    fn message_type_parses_json() {
        assert_eq!(message_type(r#"{"type":"blur","key":"j"}"#).as_deref(), Some("blur"));
        assert_eq!(message_type("not json"), None);
    }
}
