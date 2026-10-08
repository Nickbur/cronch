//! macOS "reopen": clicking Cronch's Dock icon, or opening the app again from
//! Finder, Launchpad or Spotlight while it runs, does not start a second
//! process — the system sends the running instance a reopen Apple event.
//! Cronch answers it by bringing its window up. (A second *process*, e.g. the
//! binary started again from a terminal, takes the show-request path in
//! `main.rs` instead.)
//!
//! winit does not surface this event, so a handler is registered with
//! `NSAppleEventManager` directly. It must be installed once the event loop is
//! running: AppKit registers its own default handler while launching, and ours
//! has to replace it. Other platforms have no such event; there both functions
//! are no-ops.

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AnyThread, class, define_class, msg_send, sel};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set by the Apple event handler (on the main thread), taken by the UI loop.
    static REQUESTED: AtomicBool = AtomicBool::new(false);

    /// `kCoreEventClass` ('aevt') and `kAEReopenApplication` ('rapp').
    const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
    const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and this class
        // does not implement Drop.
        #[unsafe(super(NSObject))]
        #[name = "CronchReopenHandler"]
        struct ReopenHandler;

        impl ReopenHandler {
            #[unsafe(method(handleReopen:withReplyEvent:))]
            fn handle_reopen(&self, _event: &AnyObject, _reply: &AnyObject) {
                REQUESTED.store(true, Ordering::Relaxed);
            }
        }
    );

    pub fn install() {
        let this = ReopenHandler::alloc().set_ivars(());
        // SAFETY: plain NSObject initialiser.
        let handler: Retained<ReopenHandler> = unsafe { msg_send![super(this), init] };
        // SAFETY: documented AppKit API; the handler implements the selector
        // with the expected (event, reply) signature, and both event codes
        // are FourCharCode (u32) values.
        unsafe {
            let manager: Retained<AnyObject> =
                msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            let _: () = msg_send![
                &manager,
                setEventHandler: &*handler,
                andSelector: sel!(handleReopen:withReplyEvent:),
                forEventClass: CORE_EVENT_CLASS,
                andEventID: REOPEN_APPLICATION
            ];
        }
        // The manager does not retain its handlers: keep this one for the
        // lifetime of the app.
        std::mem::forget(handler);
    }

    pub fn take_request() -> bool {
        REQUESTED.swap(false, Ordering::Relaxed)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn install() {}

    pub fn take_request() -> bool {
        false
    }
}

pub use imp::{install, take_request};
