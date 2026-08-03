//! Mapping between WebView2 resource contexts and the vocabulary the filter
//! engine speaks.
//!
//! This is deliberately a portable enum rather than the raw
//! `COREWEBVIEW2_WEB_RESOURCE_CONTEXT` constant, so the classification rules —
//! the part that is easy to get subtly wrong — can be unit tested on any host.
//! The conversion from the Win32 constant lives in `from_webview2`, behind a
//! `cfg(windows)`.

/// The resource contexts WebView2 can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Document,
    Stylesheet,
    Image,
    Media,
    Font,
    Script,
    XmlHttpRequest,
    Fetch,
    TextTrack,
    EventSource,
    WebSocket,
    Manifest,
    SignedExchange,
    Ping,
    CspViolationReport,
    Other,
}

impl ResourceKind {
    /// The request-type string the `adblock` crate expects.
    ///
    /// `fetch` deliberately reports as `xhr`: filter lists written for uBlock
    /// Origin use `$xmlhttprequest` for both, so mapping fetch to anything else
    /// would silently miss a large share of tracker rules.
    pub fn adblock_type(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Stylesheet => "stylesheet",
            Self::Image => "image",
            Self::Media => "media",
            Self::Font => "font",
            Self::Script => "script",
            Self::XmlHttpRequest | Self::Fetch => "xhr",
            Self::WebSocket => "websocket",
            Self::Ping => "ping",
            Self::CspViolationReport => "csp_report",
            // No distinct filter semantics; "other" is what uBlock uses.
            Self::TextTrack | Self::EventSource | Self::Manifest | Self::SignedExchange
            | Self::Other => "other",
        }
    }

    /// Whether we register a `WebResourceRequested` filter for this context.
    ///
    /// Every context we opt into makes the renderer wait on a cross-process
    /// hop into our UI thread for each matching request, so this list is the
    /// main CPU lever in the blocker. Contexts excluded here carry effectively
    /// no ad or tracker traffic but plenty of volume.
    pub fn should_intercept(self) -> bool {
        match self {
            // The bulk of ads and trackers.
            Self::Script
            | Self::XmlHttpRequest
            | Self::Fetch
            | Self::Image
            | Self::Media
            | Self::WebSocket
            | Self::Ping
            | Self::CspViolationReport => true,

            // Iframes arrive as documents; ad iframes are worth the hop.
            Self::Document => true,

            // High volume, negligible tracker share. Blocking a stylesheet or
            // font mostly just breaks layout.
            Self::Stylesheet | Self::Font => false,

            // Rare and never ad-bearing.
            Self::TextTrack
            | Self::EventSource
            | Self::Manifest
            | Self::SignedExchange
            | Self::Other => false,
        }
    }

    /// Contexts to register filters for, in one place so the interceptor and
    /// its tests cannot drift apart.
    pub fn intercepted() -> impl Iterator<Item = ResourceKind> {
        ALL.iter().copied().filter(|kind| kind.should_intercept())
    }
}

const ALL: &[ResourceKind] = &[
    ResourceKind::Document,
    ResourceKind::Stylesheet,
    ResourceKind::Image,
    ResourceKind::Media,
    ResourceKind::Font,
    ResourceKind::Script,
    ResourceKind::XmlHttpRequest,
    ResourceKind::Fetch,
    ResourceKind::TextTrack,
    ResourceKind::EventSource,
    ResourceKind::WebSocket,
    ResourceKind::Manifest,
    ResourceKind::SignedExchange,
    ResourceKind::Ping,
    ResourceKind::CspViolationReport,
    ResourceKind::Other,
];

/// How a document-context request should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentRole {
    /// The tab's own top-level navigation.
    MainFrame,
    /// An embedded frame.
    SubFrame,
}

/// Decide what a request in `Document` context actually is.
///
/// WebView2 reports both the top-level navigation and every iframe as
/// `DOCUMENT`. We only ever want to block the latter here: a main-frame
/// decision belongs in `NavigationStarting`, where we can show a real block
/// page instead of leaving the user on a blank tab.
pub fn classify_document(request_url: &str, pending_main_frame: Option<&str>) -> DocumentRole {
    match pending_main_frame {
        Some(pending) if urls_equivalent(pending, request_url) => DocumentRole::MainFrame,
        _ => DocumentRole::SubFrame,
    }
}

/// Compare two URLs ignoring the fragment, which never reaches the network.
fn urls_equivalent(a: &str, b: &str) -> bool {
    a.split('#').next() == b.split('#').next()
}

/// The filter type to hand the engine for a request, or `None` when the
/// request must not be evaluated here at all.
pub fn filter_type_for(
    kind: ResourceKind,
    request_url: &str,
    pending_main_frame: Option<&str>,
) -> Option<&'static str> {
    if !kind.should_intercept() {
        return None;
    }
    if kind == ResourceKind::Document {
        return match classify_document(request_url, pending_main_frame) {
            DocumentRole::MainFrame => None,
            DocumentRole::SubFrame => Some("sub_frame"),
        };
    }
    Some(kind.adblock_type())
}

#[cfg(windows)]
mod win {
    use super::ResourceKind;
    use webview2_com::Microsoft::Web::WebView2::Win32::*;

    impl ResourceKind {
        pub fn from_webview2(context: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> Self {
            match context {
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => Self::Document,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => Self::Stylesheet,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => Self::Image,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => Self::Media,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => Self::Font,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => Self::Script,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => Self::XmlHttpRequest,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => Self::Fetch,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_TEXT_TRACK => Self::TextTrack,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_EVENT_SOURCE => Self::EventSource,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => Self::WebSocket,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MANIFEST => Self::Manifest,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SIGNED_EXCHANGE => Self::SignedExchange,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => Self::Ping,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_CSP_VIOLATION_REPORT => Self::CspViolationReport,
                _ => Self::Other,
            }
        }

        pub fn to_webview2(self) -> COREWEBVIEW2_WEB_RESOURCE_CONTEXT {
            match self {
                Self::Document => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
                Self::Stylesheet => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET,
                Self::Image => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
                Self::Media => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA,
                Self::Font => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT,
                Self::Script => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
                Self::XmlHttpRequest => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
                Self::Fetch => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
                Self::TextTrack => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_TEXT_TRACK,
                Self::EventSource => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_EVENT_SOURCE,
                Self::WebSocket => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
                Self::Manifest => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MANIFEST,
                Self::SignedExchange => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SIGNED_EXCHANGE,
                Self::Ping => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING,
                Self::CspViolationReport => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_CSP_VIOLATION_REPORT,
                Self::Other => COREWEBVIEW2_WEB_RESOURCE_CONTEXT_OTHER,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_and_xhr_share_a_filter_type() {
        // Filter lists use $xmlhttprequest for both.
        assert_eq!(ResourceKind::Fetch.adblock_type(), "xhr");
        assert_eq!(ResourceKind::XmlHttpRequest.adblock_type(), "xhr");
    }

    #[test]
    fn high_volume_low_risk_contexts_are_not_intercepted() {
        assert!(!ResourceKind::Stylesheet.should_intercept());
        assert!(!ResourceKind::Font.should_intercept());
        assert!(!ResourceKind::TextTrack.should_intercept());
        assert!(!ResourceKind::Other.should_intercept());
    }

    #[test]
    fn ad_bearing_contexts_are_intercepted() {
        for kind in [
            ResourceKind::Script,
            ResourceKind::XmlHttpRequest,
            ResourceKind::Fetch,
            ResourceKind::Image,
            ResourceKind::Ping,
            ResourceKind::Document,
        ] {
            assert!(kind.should_intercept(), "{kind:?} should be intercepted");
        }
    }

    #[test]
    fn intercepted_list_matches_the_predicate() {
        let listed: Vec<_> = ResourceKind::intercepted().collect();
        assert!(!listed.is_empty());
        assert!(listed.iter().all(|k| k.should_intercept()));
        assert_eq!(listed.len(), ALL.iter().filter(|k| k.should_intercept()).count());
    }

    #[test]
    fn the_top_level_navigation_is_not_blocked_here() {
        let pending = Some("https://news.test/article");
        assert_eq!(
            classify_document("https://news.test/article", pending),
            DocumentRole::MainFrame
        );
        // Main-frame decisions belong to NavigationStarting.
        assert_eq!(filter_type_for(ResourceKind::Document, "https://news.test/article", pending), None);
    }

    #[test]
    fn embedded_frames_are_filtered_as_subframes() {
        let pending = Some("https://news.test/article");
        assert_eq!(
            classify_document("https://ads.test/frame.html", pending),
            DocumentRole::SubFrame
        );
        assert_eq!(
            filter_type_for(ResourceKind::Document, "https://ads.test/frame.html", pending),
            Some("sub_frame")
        );
    }

    #[test]
    fn a_fragment_does_not_make_a_navigation_look_like_a_frame() {
        let pending = Some("https://news.test/article");
        assert_eq!(
            classify_document("https://news.test/article#section", pending),
            DocumentRole::MainFrame
        );
    }

    #[test]
    fn documents_are_subframes_when_no_navigation_is_pending() {
        // Nothing pending means the main frame already committed, so any
        // document request now is an embedded one.
        assert_eq!(classify_document("https://ads.test/f.html", None), DocumentRole::SubFrame);
    }

    #[test]
    fn non_intercepted_contexts_yield_no_filter_type() {
        assert_eq!(filter_type_for(ResourceKind::Font, "https://x.test/f.woff", None), None);
        assert_eq!(
            filter_type_for(ResourceKind::Script, "https://x.test/a.js", None),
            Some("script")
        );
    }
}
