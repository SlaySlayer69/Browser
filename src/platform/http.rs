//! A minimal HTTPS GET over WinHTTP.
//!
//! Filter lists are the only thing this browser fetches outside a WebView, and
//! they are fetched a few times a week. Pulling in a full async HTTP client and
//! a second TLS stack for that would add megabytes to the binary and a
//! certificate store that disagrees with the one Edge uses. WinHTTP is already
//! in the OS, validates against the system trust store, and honours the user's
//! proxy configuration.

use std::ffi::c_void;

use windows::core::{Error, Result, HRESULT, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

/// Refuse to buffer more than this from one list. EasyList is ~4 MB; anything
/// far past that is a misconfigured URL, not a filter list.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

const TIMEOUT_MS: i32 = 30_000;

/// Owns a WinHTTP handle and closes it on drop, including on early return.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

impl Handle {
    fn new(raw: *mut c_void) -> Result<Self> {
        if raw.is_null() {
            Err(Error::from_thread())
        } else {
            Ok(Self(raw))
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Fetch `url` and return the body as text.
///
/// Only 200 responses yield a body; anything else is an error, because a
/// redirect page or an error document silently parsed as a filter list would
/// produce an engine that blocks nothing.
pub fn get_text(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url)
        .map_err(|_| Error::new(HRESULT(-1), "filter list URL could not be parsed"))?;
    if parsed.scheme() != "https" {
        // Filter lists steer what the browser blocks; fetching them over plain
        // HTTP would let anyone on the path rewrite the rules.
        return Err(Error::new(HRESULT(-1), "filter lists must be fetched over https"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::new(HRESULT(-1), "filter list URL has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut path = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }

    // Bind every wide string to a local: passing `wide(..).as_ptr()` inline
    // relies on temporary lifetimes that a later refactor could silently break.
    let agent = wide("CleanDark/0.1");
    let host_w = wide(host);
    let verb = wide("GET");
    let path_w = wide(&path);

    unsafe {
        let session = Handle::new(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;

        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)?;

        let connect = Handle::new(WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            port,
            0,
        ))?;

        let request = Handle::new(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        ))?;

        WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let status = query_status(request.0)?;
        if status != 200 {
            return Err(Error::new(
                HRESULT(-1),
                format!("filter list request returned HTTP {status}"),
            ));
        }

        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut available: u32 = 0;
            WinHttpQueryDataAvailable(request.0, &mut available)?;
            if available == 0 {
                break;
            }

            if body.len() + available as usize > MAX_BODY_BYTES {
                return Err(Error::new(HRESULT(-1), "filter list exceeds the size limit"));
            }

            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read: u32 = 0;
            WinHttpReadData(
                request.0,
                body[start..].as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            )?;
            body.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }

        // Filter lists are UTF-8 by convention; a stray invalid byte should
        // cost one rule, not the whole update.
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

unsafe fn query_status(request: *mut c_void) -> Result<u32> {
    let mut status: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    WinHttpQueryHeaders(
        request,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        PCWSTR::null(),
        Some(&mut status as *mut u32 as *mut c_void),
        &mut size,
        std::ptr::null_mut(),
    )?;
    Ok(status)
}
