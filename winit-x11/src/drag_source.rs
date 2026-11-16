//! Source side of the XDND protocol. Receiving drops is handled by the event processor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit_core::cursor::CursorIcon;
use winit_core::data_transfer::{DataTransferId, DataTransferSend, SendData};
use winit_core::event::WindowEvent;
use winit_core::event_loop::DndAction;
use winit_core::window::WindowId;
use x11_dl::xlib::XSelectionRequestEvent;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask};
use x11rb::{CURRENT_TIME, NONE};

use crate::atoms::*;
use crate::data_transfer::{SelectionType, encode_uri_list, generate_transfer_id};
use crate::event_loop::X11Error;
use crate::xdisplay::XConnection;

const VERSION: u32 = 5;
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
fn grab_pointer(
    xconn: &XConnection,
    source: xproto::Window,
    device: xinput::DeviceId,
    icon: CursorIcon,
) -> Result<xproto::GrabStatus, X11Error> {
    let mask = xinput::XIEventMask::BUTTON_RELEASE | xinput::XIEventMask::MOTION;
    let cursor = xconn.cached_cursor(Some(icon)).unwrap_or(NONE);

    let reply = xconn
        .xcb_connection()
        .xinput_xi_grab_device(
            source,
            CURRENT_TIME,
            cursor,
            device,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
            xinput::GrabOwner::NO_OWNER,
            &[mask.into()],
        )?
        .reply()?;
    Ok(reply.status)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    window: xproto::Window,
    recipient: xproto::Window,
    version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    x: i16,
    y: i16,
    time: u32,
}

impl Position {
    fn coordinates(self) -> u32 {
        (u32::from(self.x as u16) << 16) | u32::from(self.y as u16)
    }
}

#[derive(Debug)]
struct Hover {
    target: Target,
    pending: Option<Position>,
    waiting: bool,
    accepted: bool,
}

#[derive(Debug)]
pub(crate) struct DragSource {
    xconn: Arc<XConnection>,
    id: DataTransferId,
    source: xproto::Window,
    data: Box<dyn DataTransferSend>,
    types: Vec<xproto::Atom>,
    root: xproto::Window,
    device: xinput::DeviceId,
    source_borderless: bool,
    hover: Option<Hover>,
    pointer_grabbed: bool,
    owns_selection: bool,
    release: Option<u32>,
    dropped: bool,
    deadline: Option<Instant>,
}

impl DragSource {
    pub(crate) fn start(
        xconn: Arc<XConnection>,
        source: xproto::Window,
        root: xproto::Window,
        device: xinput::DeviceId,
        source_borderless: bool,
        data: Box<dyn DataTransferSend>,
    ) -> Result<Self, X11Error> {
        let atoms = xconn.atoms();
        let conn = xconn.xcb_connection();
        let pointer = xconn.query_pointer(root, device)?;
        if !pointer.same_screen || !pointer.buttons.first().is_some_and(|buttons| buttons & 2 != 0)
        {
            return Err(X11Error::UnexpectedNull("active primary button drag"));
        }

        let types = advertised_types(&xconn, &*data);
        if types.is_empty() {
            return Err(X11Error::UnexpectedNull("supported XDND data types"));
        }

        // Button presses reported by XI2 already establish an implicit XI2 grab.
        if grab_pointer(&xconn, source, device, CursorIcon::NoDrop)? != xproto::GrabStatus::SUCCESS
        {
            return Err(X11Error::UnexpectedNull("XDND pointer grab"));
        }

        let selection = atoms[XdndSelection];
        let mut drag = Self {
            xconn: Arc::clone(&xconn),
            id: generate_transfer_id(),
            source,
            data,
            types,
            root,
            device,
            source_borderless,
            hover: None,
            pointer_grabbed: true,
            owns_selection: false,
            release: None,
            dropped: false,
            deadline: None,
        };

        conn.set_selection_owner(source, selection, CURRENT_TIME)?.check()?;
        drag.owns_selection = conn.get_selection_owner(selection)?.reply()?.owner == source;
        if !drag.owns_selection {
            return Err(X11Error::UnexpectedNull("XDND selection owner"));
        }
        if drag.types.len() > 3 {
            xconn
                .change_property(
                    source,
                    atoms[XdndTypeList],
                    xproto::AtomEnum::ATOM.into(),
                    xproto::PropMode::REPLACE,
                    &drag.types,
                )?
                .check()?;
        }

        drag.motion(
            (pointer.root_x >> 16) as i16,
            (pointer.root_y >> 16) as i16,
            xconn.timestamp(),
        )?;
        Ok(drag)
    }

    pub(crate) fn id(&self) -> DataTransferId {
        self.id
    }

    pub(crate) fn device_matches(&self, device: i32) -> bool {
        self.pointer_grabbed && i32::from(self.device) == device
    }

    pub(crate) fn source(&self) -> xproto::Window {
        self.source
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn window_destroyed(&self, window: xproto::Window) -> bool {
        self.source == window
            || self.hover.as_ref().is_some_and(|hover| hover.target.window == window)
    }

    pub(crate) fn motion(&mut self, x: i16, y: i16, time: u32) -> Result<(), X11Error> {
        if self.release.is_some() {
            return Ok(());
        }
        let position = Position { x, y, time };
        let target = target_at(
            &self.xconn,
            self.root,
            position,
            self.source_borderless.then_some(self.source),
        );

        if self.hover.as_ref().map(|hover| hover.target) != target {
            self.leave();
            if let Some(target) = target {
                let mut data = [self.source, target.version << 24, NONE, NONE, NONE];
                data[1] |= u32::from(self.types.len() > 3);
                for (slot, ty) in data[2..].iter_mut().zip(&self.types) {
                    *slot = *ty;
                }
                self.send(target, self.xconn.atoms()[XdndEnter], data)?;
                self.hover = Some(Hover { target, pending: None, waiting: false, accepted: false });
            }
        }

        if let Some(hover) = self.hover.as_mut() {
            hover.pending = Some(position);
            self.send_position()?;
        }
        Ok(())
    }

    fn send_position(&mut self) -> Result<(), X11Error> {
        let Some(hover) = self.hover.as_mut() else { return Ok(()) };
        if hover.waiting {
            return Ok(());
        }
        let Some(position) = hover.pending.take() else { return Ok(()) };
        let target = hover.target;
        hover.waiting = true;
        self.send(target, self.xconn.atoms()[XdndPosition], [
            self.source,
            0,
            position.coordinates(),
            position.time,
            self.xconn.atoms()[XdndActionCopy],
        ])
    }

    pub(crate) fn release(&mut self, time: u32) -> Result<Option<bool>, X11Error> {
        if self.release.is_some() {
            return Ok(None);
        }
        self.release = Some(time);
        self.release_pointer();
        self.deadline = Some(Instant::now() + REPLY_TIMEOUT);
        self.maybe_drop()
    }

    fn maybe_drop(&mut self) -> Result<Option<bool>, X11Error> {
        let Some(time) = self.release else { return Ok(None) };
        let Some(hover) = self.hover.as_ref() else { return Ok(Some(false)) };
        if self.dropped || hover.waiting {
            return Ok(None);
        }
        if !hover.accepted {
            return Ok(Some(false));
        }
        self.send(hover.target, self.xconn.atoms()[XdndDrop], [self.source, 0, time, 0, 0])?;
        self.dropped = true;
        self.deadline = Some(Instant::now() + REPLY_TIMEOUT);
        Ok(None)
    }

    pub(crate) fn status(
        &mut self,
        target: u32,
        flags: u32,
        action: u32,
    ) -> Result<Option<bool>, X11Error> {
        let Some(hover) = self.hover.as_mut() else { return Ok(None) };
        if self.dropped || !hover.waiting || hover.target.window != target {
            return Ok(None);
        }
        hover.waiting = false;
        let accepted = flags & 1 != 0 && action == self.xconn.atoms()[XdndActionCopy];
        if std::mem::replace(&mut hover.accepted, accepted) != accepted {
            self.update_cursor(accepted);
        }
        self.send_position()?;
        self.maybe_drop()
    }

    pub(crate) fn finished(&self, target: u32, flags: u32, action: u32) -> Option<bool> {
        let hover = self.hover.as_ref()?;
        (self.dropped && target == hover.target.window).then(|| {
            hover.target.version < 5
                || (flags & 1 != 0 && action == self.xconn.atoms()[XdndActionCopy])
        })
    }

    pub(crate) fn finish(mut self, success: bool) -> (WindowId, WindowEvent) {
        if !success && !self.dropped {
            self.leave();
        }
        let id = self.id;
        let event = if success {
            WindowEvent::OutgoingDragDropped { id, action: Some(DndAction::Copy) }
        } else {
            WindowEvent::OutgoingDragCanceled { id }
        };
        (WindowId::from_raw(self.source as _), event)
    }

    pub(crate) fn selection_request(&self, request: &XSelectionRequestEvent) {
        let conn = self.xconn.xcb_connection();
        let requestor = request.requestor as _;
        let target = request.target as _;
        let property = if request.property == 0 { target } else { request.property as _ };
        let result = if request.owner == self.source as _ && self.dropped {
            self.write_selection(requestor, target, property)
        } else {
            Ok(false)
        };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time as _,
            requestor,
            selection: request.selection as _,
            target,
            property: if matches!(result, Ok(true)) { property } else { NONE },
        };
        if let Err(err) = result {
            debug!("Could not write XDND selection: {err}");
        }
        if let Ok(cookie) = conn.send_event(false, requestor, EventMask::NO_EVENT, notify) {
            cookie.ignore_error();
        }
    }

    fn write_selection(
        &self,
        requestor: xproto::Window,
        target: xproto::Atom,
        property: xproto::Atom,
    ) -> Result<bool, X11Error> {
        let atoms = self.xconn.atoms();
        if target == atoms[TARGETS] {
            let mut types = vec![atoms[TARGETS]];
            types.extend(&self.types);
            self.xconn
                .change_property(
                    requestor,
                    property,
                    xproto::AtomEnum::ATOM.into(),
                    xproto::PropMode::REPLACE,
                    &types,
                )?
                .check()?;
            return Ok(true);
        }
        if !self.types.contains(&target) {
            return Ok(false);
        }
        let Some(data) = self.data.data_for_type(&SelectionType::new(atoms, target)) else {
            return Ok(false);
        };
        let bytes = match data {
            SendData::Uris(uris) => encode_uri_list(uris),
            SendData::String(value) => value.into_bytes(),
            SendData::Bytes(value) => value,
            _ => return Ok(false),
        };
        // This source sends a single property. Larger selections need the ICCCM INCR protocol.
        if bytes.len() > self.xconn.xcb_connection().maximum_request_bytes().saturating_sub(32) {
            return Ok(false);
        }
        self.xconn
            .change_property(requestor, property, target, xproto::PropMode::REPLACE, &bytes)?
            .check()?;
        Ok(true)
    }

    fn send(&self, target: Target, message: xproto::Atom, data: [u32; 5]) -> Result<(), X11Error> {
        self.xconn
            .send_client_msg(target.window, target.recipient, message, None, data)?
            .ignore_error();
        self.xconn.xcb_connection().flush()?;
        Ok(())
    }

    fn leave(&mut self) {
        if let Some(hover) = self.hover.take() {
            let _ =
                self.send(hover.target, self.xconn.atoms()[XdndLeave], [self.source, 0, 0, 0, 0]);
            self.update_cursor(false);
        }
    }

    // Both windows can belong to this process. The destination may change
    // its decision after XdndStatus (for example after an application frame)
    // without the pointer moving again.
    pub(crate) fn update_local_feedback(
        &self,
        source: xproto::Window,
        target: xproto::Window,
        accepted: bool,
    ) {
        if self.source == source
            && self.hover.as_ref().is_some_and(|hover| hover.target.window == target)
        {
            self.update_cursor(accepted);
        }
    }

    fn update_cursor(&self, accepted: bool) {
        if self.pointer_grabbed {
            let icon = if accepted { CursorIcon::Copy } else { CursorIcon::NoDrop };
            let _ = grab_pointer(&self.xconn, self.source, self.device, icon);
        }
    }

    fn release_pointer(&mut self) {
        if std::mem::take(&mut self.pointer_grabbed) {
            if let Ok(cookie) =
                self.xconn.xcb_connection().xinput_xi_ungrab_device(CURRENT_TIME, self.device)
            {
                cookie.ignore_error();
            }
        }
    }
}

impl Drop for DragSource {
    fn drop(&mut self) {
        self.release_pointer();
        let conn = self.xconn.xcb_connection();
        let selection = self.xconn.atoms()[XdndSelection];
        if self.owns_selection
            && conn
                .get_selection_owner(selection)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|reply| reply.owner == self.source)
        {
            if let Ok(cookie) = conn.set_selection_owner(NONE, selection, CURRENT_TIME) {
                cookie.ignore_error();
            }
        }
        let _ = conn.flush();
    }
}

fn advertised_types(xconn: &XConnection, data: &dyn DataTransferSend) -> Vec<xproto::Atom> {
    let mut types = Vec::new();
    data.for_each_available_type(&mut |ty| {
        for ty in SelectionType::from_dyn(xconn.atoms(), ty) {
            let atom = ty.atom();
            if !types.contains(&atom) {
                types.push(atom);
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    types
}

fn target_at(
    xconn: &XConnection,
    root: xproto::Window,
    position: Position,
    borderless_source: Option<xproto::Window>,
) -> Option<Target> {
    let conn = xconn.xcb_connection();
    let frame = child_at(xconn, root, root, position)?;
    if let Some(target) = target_in_frame(xconn, root, frame, position) {
        return Some(target);
    }

    // A window manager can wrap even a borderless client in a frame whose
    // shadow covers its neighbors. Only look behind the source's own wrapper,
    // and stop at the first lower window that actually covers the pointer.
    let source = borderless_source?;
    if conn.query_tree(frame).ok()?.reply().ok()?.children.as_slice() != [source] {
        return None;
    }
    let children = conn.query_tree(root).ok()?.reply().ok()?.children;
    let covered = children
        .into_iter()
        .rev()
        .skip_while(|&window| window != frame)
        .skip(1)
        .find(|&window| window_contains_point(xconn, window, position))?;
    target_in_frame(xconn, root, covered, position)
}

fn target_in_frame(
    xconn: &XConnection,
    root: xproto::Window,
    mut window: xproto::Window,
    position: Position,
) -> Option<Target> {
    let conn = xconn.xcb_connection();
    let atoms = xconn.atoms();
    let property = |window, name, ty: xproto::AtomEnum| {
        conn.get_property(false, window, atoms[name], ty, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    };

    for _ in 0..64 {
        let proxy = property(window, XdndProxy, xproto::AtomEnum::WINDOW)
            .filter(|&proxy| property(proxy, XdndProxy, xproto::AtomEnum::WINDOW) == Some(proxy));
        let recipient = proxy.unwrap_or(window);
        if let Some(version) =
            property(recipient, XdndAware, xproto::AtomEnum::ATOM).filter(|&version| version >= 3)
        {
            return Some(Target { window, recipient, version: version.min(VERSION) });
        }
        window = child_at(xconn, root, window, position)?;
    }
    None
}

fn child_at(
    xconn: &XConnection,
    root: xproto::Window,
    parent: xproto::Window,
    position: Position,
) -> Option<xproto::Window> {
    let reply = xconn
        .xcb_connection()
        .translate_coordinates(root, parent, position.x, position.y)
        .ok()?
        .reply()
        .ok()?;
    (reply.same_screen && reply.child != NONE).then_some(reply.child)
}

fn window_contains_point(xconn: &XConnection, window: xproto::Window, position: Position) -> bool {
    let conn = xconn.xcb_connection();
    let Some(geometry) = conn.get_geometry(window).ok().and_then(|cookie| cookie.reply().ok())
    else {
        return false;
    };
    let border = i32::from(geometry.border_width);
    let x = i32::from(position.x) - i32::from(geometry.x) - border;
    let y = i32::from(position.y) - i32::from(geometry.y) - border;
    if !(-border..i32::from(geometry.width) + border).contains(&x)
        || !(-border..i32::from(geometry.height) + border).contains(&y)
        || !conn
            .get_window_attributes(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attrs| attrs.map_state == xproto::MapState::VIEWABLE)
    {
        return false;
    }
    if conn.extension_information(shape::X11_EXTENSION_NAME).ok().flatten().is_none() {
        return true;
    }
    [shape::SK::BOUNDING, shape::SK::INPUT].into_iter().all(|kind| {
        conn.shape_get_rectangles(window, kind)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| {
                reply.rectangles.iter().any(|rect| {
                    let left = i32::from(rect.x);
                    let top = i32::from(rect.y);
                    (left..left + i32::from(rect.width)).contains(&x)
                        && (top..top + i32::from(rect.height)).contains(&y)
                })
            })
    })
}
