//! WebKit load-state signals wry does not surface.
//!
//! wry reports a page's commit and finish but neither a failed navigation nor
//! load progress. [`LoadObserver`] watches the webview's KVO-compliant
//! `loading` and `estimatedProgress`; [`NavigationRelay`] stands in as its
//! navigation delegate to receive failures, forwarding everything else to
//! wry's delegate. Kept free of GPUI so the pair can be exercised against a
//! real webview outside the app.

use std::ffi::c_void;
use std::ptr::null_mut;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{AllocAnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{
    NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions,
    NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSString, NSURL, ns_string,
};
use objc2_web_kit::{WKNavigation, WKNavigationDelegate, WKWebView};

pub struct LoadObserverIvars {
    webview: Retained<WKWebView>,
    handler: Box<dyn Fn()>,
}

define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[ivars = LoadObserverIvars]
    pub struct LoadObserver;

    /// NSKeyValueObserving on the webview's `loading` and
    /// `estimatedProgress`, both documented KVO-compliant. They are the
    /// push signals for load state: wry reports commits and finishes but
    /// not failures, so a load that never commits left the toolbar
    /// "loading" forever, and progress used to be polled every display
    /// frame for as long as that lasted.
    impl LoadObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_key_path(
            &self,
            _key_path: Option<&NSString>,
            _of_object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut c_void,
        ) {
            (self.ivars().handler)();
        }
    }

    unsafe impl NSObjectProtocol for LoadObserver {}
);

const LOAD_KEY_PATHS: [&str; 2] = ["loading", "estimatedProgress"];

impl LoadObserver {
    pub fn new(webview: Retained<WKWebView>, handler: Box<dyn Fn()>) -> Retained<Self> {
        let observer = Self::alloc().set_ivars(LoadObserverIvars { webview, handler });
        let observer: Retained<Self> = unsafe { msg_send![super(observer), init] };
        for key_path in LOAD_KEY_PATHS {
            unsafe {
                observer
                    .ivars()
                    .webview
                    .addObserver_forKeyPath_options_context(
                        &observer,
                        &NSString::from_str(key_path),
                        NSKeyValueObservingOptions::New,
                        null_mut(),
                    );
            }
        }
        observer
    }
}

impl Drop for LoadObserver {
    fn drop(&mut self) {
        for key_path in LOAD_KEY_PATHS {
            unsafe {
                self.ivars()
                    .webview
                    .removeObserver_forKeyPath(self, &NSString::from_str(key_path));
            }
        }
    }
}

/// A main-frame navigation that failed, in WebKit's own words.
pub struct NavigationError {
    pub domain: String,
    pub code: isize,
    pub url: Option<String>,
    pub message: String,
    /// Whether the page had committed: a provisional failure never showed
    /// anything, while a committed one leaves partial content on screen.
    pub provisional: bool,
}

impl NavigationError {
    fn from_ns_error(error: &NSError, provisional: bool) -> Self {
        let user_info = error.userInfo();
        let url = user_info
            .objectForKey(ns_string!("NSErrorFailingURLStringKey"))
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|value| value.to_string())
            .or_else(|| {
                user_info
                    .objectForKey(ns_string!("NSErrorFailingURLKey"))
                    .and_then(|value| value.downcast::<NSURL>().ok())
                    .and_then(|value| value.absoluteString())
                    .map(|value| value.to_string())
            });
        Self {
            domain: error.domain().to_string(),
            code: error.code(),
            url,
            message: error.localizedDescription().to_string(),
            provisional,
        }
    }
}

pub struct NavigationRelayIvars {
    inner: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
    on_failure: Box<dyn Fn(NavigationError)>,
}

define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationRelayIvars]
    pub struct NavigationRelay;

    /// Stands in as the webview's navigation delegate so failures reach
    /// Waku. wry's own delegate implements the policy, commit, finish
    /// and download callbacks and none of the failure ones; every
    /// selector this class lacks is forwarded to it unchanged, and
    /// `respondsToSelector:` answers for both, so WebKit sees one
    /// delegate with the union of their methods.
    impl NavigationRelay {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            own || self.ivars().inner.respondsToSelector(selector)
        }

        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target_for_selector(&self, selector: Sel) -> *mut AnyObject {
            let inner = &self.ivars().inner;
            if inner.respondsToSelector(selector) {
                Retained::as_ptr(inner) as *mut AnyObject
            } else {
                null_mut()
            }
        }
    }

    unsafe impl NSObjectProtocol for NavigationRelay {}

    unsafe impl WKNavigationDelegate for NavigationRelay {
        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            web_view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let inner = &self.ivars().inner;
            if inner.respondsToSelector(sel!(webView:didFailProvisionalNavigation:withError:)) {
                unsafe {
                    inner.webView_didFailProvisionalNavigation_withError(
                        web_view, navigation, error,
                    );
                }
            }
            (self.ivars().on_failure)(NavigationError::from_ns_error(error, true));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(
            &self,
            web_view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let inner = &self.ivars().inner;
            if inner.respondsToSelector(sel!(webView:didFailNavigation:withError:)) {
                unsafe {
                    inner.webView_didFailNavigation_withError(web_view, navigation, error);
                }
            }
            (self.ivars().on_failure)(NavigationError::from_ns_error(error, false));
        }
    }
);

impl NavigationRelay {
    /// Installs a relay as `webview`'s navigation delegate, in front of the
    /// one already there. `None` if there was no delegate to forward to. The
    /// webview holds delegates weakly: keep the relay alive while installed.
    pub fn install(
        webview: &WKWebView,
        on_failure: Box<dyn Fn(NavigationError)>,
    ) -> Option<Retained<Self>> {
        let inner = unsafe { webview.navigationDelegate() }?;
        let relay =
            Self::alloc(webview.mtm()).set_ivars(NavigationRelayIvars { inner, on_failure });
        let relay: Retained<Self> = unsafe { msg_send![super(relay), init] };
        unsafe { webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*relay))) };
        Some(relay)
    }

    /// Puts the forwarded delegate back on `webview`.
    pub fn uninstall(&self, webview: &WKWebView) {
        unsafe { webview.setNavigationDelegate(Some(&self.ivars().inner)) };
    }
}
