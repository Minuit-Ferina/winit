//! Resolve a portal capability after Drop, without blocking the event loop.
use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use calloop::channel;
use rustix::pipe::{self, PipeFlags};

use super::{DataFetch, MimeType, TEXT_URI_LIST};
use crate::state::WinitState;

static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct Worker;
impl Drop for Worker {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PortalError {
    Unavailable,
    InvalidKey,
    InvalidResponse,
    Failed,
}

impl PortalError {
    fn into_io(self) -> io::Error {
        let message = match self {
            Self::Unavailable => "file portal service unavailable",
            Self::InvalidKey => "invalid file portal capability",
            Self::InvalidResponse => "invalid file portal response",
            Self::Failed => "file portal transfer failed",
        };
        io::Error::other(message)
    }
}

fn service_error(name: Option<&str>) -> PortalError {
    // Only absent services or an older portal without FileTransfer permit a downgrade.
    // A refusal, invalid key, malformed reply or timeout must stay a failed transfer.
    match name {
        Some(
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.UnknownMethod",
        ) => PortalError::Unavailable,
        _ => PortalError::Failed,
    }
}

fn connection_error(name: Option<&str>) -> PortalError {
    // These transport errors are safe to retry only while connecting, before the
    // portal has received the capability. Never classify a method reply this way.
    match name {
        Some(
            "org.freedesktop.DBus.Error.NoServer"
            | "org.freedesktop.DBus.Error.FileNotFound"
            | "org.freedesktop.DBus.Error.Disconnected",
        ) => PortalError::Unavailable,
        _ => PortalError::Failed,
    }
}

fn key(bytes: &[u8]) -> Option<&str> {
    if bytes.is_empty() || bytes.len() > 4096 {
        return None;
    }
    // GTK serializes a string including its trailing C terminator; other sources omit it.
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let key = std::str::from_utf8(bytes).ok()?;
    // GTK sends the key as UTF-8, with no URI decoding or shell interpretation.
    (!key.is_empty() && !key.contains('\0') && !key.chars().any(char::is_control)).then_some(key)
}

fn encode_paths(paths: Vec<String>) -> Result<Vec<u8>, PortalError> {
    if paths.is_empty()
        || paths.len() > 256
        || paths.iter().map(String::len).sum::<usize>() > 1024 * 1024
    {
        return Err(PortalError::InvalidResponse);
    }
    let uris = paths
        .into_iter()
        .map(|p| {
            let path = PathBuf::from(&p);
            if !path.is_absolute() || p.contains('\0') {
                return Err(PortalError::InvalidResponse);
            }
            url::Url::from_file_path(path)
                .map(|uri| uri.to_string())
                .map_err(|_| PortalError::InvalidResponse)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let bytes = uris.join("\r\n").into_bytes();
    if bytes.len() > 1024 * 1024 {
        return Err(PortalError::InvalidResponse);
    }
    Ok(bytes)
}

fn retrieve(key: &str) -> Result<Vec<u8>, PortalError> {
    let connection = dbus::blocking::Connection::new_session()
        .map_err(|error| connection_error(error.name()))?;
    let proxy = connection.with_proxy(
        "org.freedesktop.portal.Documents",
        "/org/freedesktop/portal/documents",
        Duration::from_secs(5),
    );
    let (paths,): (Vec<String>,) = proxy
        .method_call(
            "org.freedesktop.portal.FileTransfer",
            "RetrieveFiles",
            (key, dbus::arg::PropMap::new()),
        )
        .map_err(|error| service_error(error.name()))?;
    encode_paths(paths)
}

fn fail(fetch: &Rc<RefCell<DataFetch>>, error: PortalError, state: &mut WinitState) {
    fetch.borrow_mut().complete(Err(Arc::new(error.into_io())), state);
}

fn retry_uri_list(
    fetch: Rc<RefCell<DataFetch>>,
    mime: MimeType,
    state: &mut WinitState,
) -> io::Result<()> {
    let (id, serial) = {
        let fetch = fetch.borrow();
        (fetch.id, fetch.serial)
    };
    let session = state.data_transfer_state.session_mut(id).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "data transfer offer is no longer available")
    })?;
    let (readfd, writefd) = pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
    DataFetch::receive(fetch, readfd, &state.loop_handle)?;
    // start_fetch keeps this serial in the same set; the failed portal attempt
    // must not complete it, mark the drag failed, or permit offer destruction.
    session.start_fetch(mime.to_string(), writefd, serial);
    Ok(())
}

pub(super) fn resolve(fetch: Rc<RefCell<DataFetch>>, bytes: Vec<u8>, state: &mut WinitState) {
    let Some(key) = key(&bytes).map(str::to_owned) else {
        fail(&fetch, PortalError::InvalidKey, state);
        return;
    };
    if WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < 8).then_some(n + 1))
        .is_err()
    {
        fail(&fetch, PortalError::Failed, state);
        return;
    }
    let worker = Worker;
    let (sender, receiver) = channel::sync_channel::<Result<Vec<u8>, PortalError>>(1);
    let receive = Rc::clone(&fetch);
    let token = match state.loop_handle.insert_source(receiver, move |event, _, state| {
        if let channel::Event::Msg(result) = event {
            let fallback = {
                let mut fetch = receive.borrow_mut();
                if fetch.mime_type.is_none() {
                    return;
                }
                if let Some(channel) = fetch.portal_channel.take() {
                    state.loop_handle.remove(channel);
                }
                result.as_ref().err().and_then(|error| fetch.start_portal_fallback(*error))
            };
            if let Some(mime) = fallback {
                tracing::debug!("File portal unavailable; receiving offered URI list");
                if let Err(error) = retry_uri_list(Rc::clone(&receive), mime, state) {
                    receive.borrow_mut().complete(Err(Arc::new(error)), state);
                }
            } else {
                let mut fetch = receive.borrow_mut();
                fetch.mime_type = Some(MimeType::parse(TEXT_URI_LIST.into()));
                fetch.complete(result.map_err(|error| Arc::new(error.into_io())), state);
            }
        }
    }) {
        Ok(token) => token,
        Err(_) => {
            fail(&fetch, PortalError::Failed, state);
            return;
        },
    };
    fetch.borrow_mut().portal_channel = Some(token);
    if std::thread::Builder::new()
        .name("wayland-file-portal".into())
        .spawn(move || {
            let _worker = worker;
            let _ = sender.send(retrieve(&key));
        })
        .is_err()
    {
        fail(&fetch, PortalError::Failed, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_capabilities_are_literal_and_bounded() {
        assert_eq!(key(b"123-abc"), Some("123-abc"));
        assert_eq!(key(b"123-abc\0"), Some("123-abc"));
        for invalid in [b"".as_slice(), b"\0", b"a\0b", b"abc\0\0", b"a\nb", &[255]] {
            assert!(key(invalid).is_none());
        }
        assert!(key(&vec![b'a'; 4097]).is_none());
    }

    #[test]
    fn only_missing_portal_services_or_interfaces_allow_fallback() {
        for name in ["ServiceUnknown", "NameHasNoOwner", "UnknownInterface", "UnknownMethod"] {
            let name = format!("org.freedesktop.DBus.Error.{name}");
            assert_eq!(service_error(Some(&name)), PortalError::Unavailable);
        }
        for name in [
            "org.freedesktop.DBus.Error.AccessDenied",
            "org.freedesktop.DBus.Error.InvalidArgs",
            "org.freedesktop.DBus.Error.FileNotFound",
            "org.freedesktop.DBus.Error.Disconnected",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.portal.Error.NotAllowed",
            "org.freedesktop.portal.Error.NotFound",
            "org.freedesktop.portal.Error.Failed",
        ] {
            assert_eq!(service_error(Some(name)), PortalError::Failed, "{name}");
        }
        assert_eq!(service_error(None), PortalError::Failed);
    }

    #[test]
    fn transport_errors_only_allow_fallback_before_calling_the_portal() {
        for name in ["NoServer", "FileNotFound", "Disconnected"] {
            let name = format!("org.freedesktop.DBus.Error.{name}");
            assert_eq!(connection_error(Some(&name)), PortalError::Unavailable);
            assert_eq!(service_error(Some(&name)), PortalError::Failed);
        }
        assert_eq!(
            connection_error(Some("org.freedesktop.DBus.Error.AccessDenied")),
            PortalError::Failed
        );
        assert_eq!(connection_error(None), PortalError::Failed);
    }

    #[test]
    fn portal_paths_are_absolute_bounded_and_uri_encoded() {
        assert_eq!(
            encode_paths(vec!["/tmp/a b#c.txt".into(), "/tmp/d.txt".into()]).unwrap(),
            b"file:///tmp/a%20b%23c.txt\r\nfile:///tmp/d.txt"
        );
        for paths in [
            vec![],
            vec!["relative.txt".into()],
            vec!["file:///tmp/a".into()],
            vec!["/tmp/a\0b".into()],
            vec!["/tmp/a".into(); 257],
            vec![format!("/{}", "a".repeat(1024 * 1024))],
            // Percent encoding must not bypass the decoded-result byte cap.
            vec![format!("/{}", " ".repeat(400_000))],
        ] {
            assert_eq!(encode_paths(paths), Err(PortalError::InvalidResponse));
        }
    }
}
