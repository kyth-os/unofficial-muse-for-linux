//! Public-surface tests needing no bus: validation runs before any I/O,
//! so these prove rejection without dialing (unroutable endpoint).
use muse_desktop::{BusEndpoint, DesktopError, DesktopPortal};

fn dead_bus() -> BusEndpoint {
    BusEndpoint::unix_socket("/nonexistent-bus-path-xyz")
}

#[test]
fn non_http_scheme_rejected_before_dial() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "javascript:alert(1)",
        "data:text/plain,hi",
    ] {
        assert_eq!(
            DesktopPortal::open_url_via(&dead_bus(), url).unwrap_err(),
            DesktopError::InvalidArgument("only http(s) urls can be opened".into()),
            "url: {url:?}"
        );
    }
}

#[test]
fn malformed_token_rejected_before_dial() {
    for token in ["", "has space", "dash-ed"] {
        assert!(
            matches!(
                DesktopPortal::screenshot_via(&dead_bus(), token).unwrap_err(),
                DesktopError::InvalidArgument(_)
            ),
            "token: {token:?}"
        );
    }
}

#[test]
fn unreachable_bus_is_transport_not_panic() {
    assert!(matches!(
        DesktopPortal::open_url_via(&dead_bus(), "https://example.com/").unwrap_err(),
        DesktopError::Transport(_)
    ));
}
