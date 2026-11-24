//! In-process drags stay in the ordinary mouse-capture message loop.

use std::rc::Rc;

use cursor_icon::CursorIcon;
use dpi::PhysicalPosition;
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows_sys::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows_sys::Win32::System::SystemServices::{
    MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_XBUTTON1, MK_XBUTTON2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, HCURSOR, SetCursor, WindowFromPoint,
};
use winit_core::data_transfer::{DataTransferId, DataTransferSend};
use winit_core::error::{NotSupportedError, TransferError};
use winit_core::event::WindowEvent;
use winit_core::window::WindowId;

use super::{Event, EventLoopRunner};
use crate::data_transfer::{
    DropEffect, SourceDataObject, drop_effect_to_dnd_action, next_data_transfer_id, pick_effect,
};
use crate::icon::load_named_cursor;
use crate::util;

const MOUSE_BUTTONS: u32 = MK_LBUTTON | MK_MBUTTON | MK_RBUTTON | MK_XBUTTON1 | MK_XBUTTON2;

pub(super) struct LocalDrag {
    source: WindowId,
    id: DataTransferId,
    offer: Rc<SourceDataObject>,
    allowed_effects: DropEffect,
    effect: DropEffect,
    target: Option<WindowId>,
    copy_cursor: HCURSOR,
    default_cursor: HCURSOR,
    refused_cursor: HCURSOR,
}

impl LocalDrag {
    fn cursor(&self) -> HCURSOR {
        match self.effect {
            DROPEFFECT_NONE => self.refused_cursor,
            DROPEFFECT_COPY => self.copy_cursor,
            _ => self.default_cursor,
        }
    }
}

impl EventLoopRunner {
    pub(super) fn start_local_drag(
        &self,
        source: WindowId,
        send_data: Box<dyn DataTransferSend>,
        allowed_effects: DropEffect,
    ) -> Result<DataTransferId, TransferError> {
        if allowed_effects == DROPEFFECT_NONE {
            return Err(NotSupportedError::new("Internal drag has no supported actions").into());
        }
        if self.source_drag.get().is_some()
            || self.pending_drag.borrow().is_some()
            || self.local_drag.borrow().is_some()
        {
            return Err(TransferError::Failed);
        }

        // SAFETY: These calls inspect capture and keyboard state on this event-loop thread.
        let has_capture = unsafe { GetCapture() == source.into_raw() as HWND };
        let pressed = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
            .into_iter()
            .any(|key| unsafe { GetKeyState(i32::from(key)) } < 0);
        if !has_capture || !pressed {
            return Err(
                NotSupportedError::new("Internal drag requires a captured mouse button").into()
            );
        }

        let id = next_data_transfer_id();
        let drag = LocalDrag {
            source,
            id,
            offer: Rc::new(SourceDataObject::for_drag(send_data)),
            allowed_effects,
            effect: DROPEFFECT_NONE,
            target: None,
            copy_cursor: load_named_cursor(CursorIcon::Copy),
            default_cursor: load_named_cursor(CursorIcon::Default),
            refused_cursor: load_named_cursor(CursorIcon::NoDrop),
        };
        set_cursor(drag.cursor());
        self.local_drag.replace(Some(drag));
        Ok(id)
    }

    pub(super) fn local_drag_motion(
        self: &Rc<Self>,
        window: HWND,
        x: i32,
        y: i32,
        keys: u32,
    ) -> bool {
        let source = WindowId::from_raw(window.addr());
        let Some(id) = self
            .local_drag
            .borrow()
            .as_ref()
            .filter(|drag| drag.source == source)
            .map(|drag| drag.id)
        else {
            return false;
        };

        let target = self.local_target_at(window, x, y);
        let (previous, offer, allowed) = {
            let mut local = self.local_drag.borrow_mut();
            let Some(drag) = local.as_mut().filter(|drag| drag.id == id) else {
                return true;
            };
            let previous = drag.target;
            drag.target = target.map(|(target, _)| target);
            (previous, drag.offer.clone(), drag.allowed_effects)
        };

        if previous != target.map(|(window, _)| window) {
            if let Some(previous) = previous {
                self.data_transfer().remove_drag(id);
                self.send_local_window_event(previous, WindowEvent::DragLeft { id });
            }
            if let Some((target, position)) = target {
                if self.local_drag.borrow().as_ref().is_some_and(|drag| drag.id == id) {
                    self.data_transfer().register_local_drag(id, target, &offer);
                    self.send_local_window_event(target, WindowEvent::DragEntered {
                        id,
                        position: Some(position),
                    });
                }
            }
        }

        if let Some((target, position)) = target {
            if self.data_transfer().drag_state(id).is_some_and(|state| state.window_id == target) {
                let proposed_action = self.data_transfer().proposed_dnd_action(id, allowed);
                self.send_local_window_event(target, WindowEvent::DragPosition {
                    id,
                    position,
                    proposed_action,
                });
            }
        }
        let effect = if target.is_some() {
            pick_effect(&self.data_transfer().current_drag_actions(id), keys, allowed)
        } else {
            DROPEFFECT_NONE
        };
        if let Some(drag) = self.local_drag.borrow_mut().as_mut().filter(|drag| drag.id == id) {
            drag.effect = effect;
            set_cursor(drag.cursor());
        }
        true
    }

    fn local_target_at(
        &self,
        source: HWND,
        x: i32,
        y: i32,
    ) -> Option<(WindowId, PhysicalPosition<f64>)> {
        let mut point = POINT { x, y };
        // SAFETY: Both coordinate calls get a live output pointer. Win32's
        // hit test doesn't send messages to windows belonging to other threads.
        let target = unsafe {
            if ClientToScreen(source, &mut point) == 0 {
                return None;
            }
            GetAncestor(WindowFromPoint(point), GA_ROOT)
        };
        let id = WindowId::from_raw(target.addr());
        if !self.data_transfer().is_local_target(id) {
            return None;
        }
        if unsafe { ScreenToClient(target, &mut point) } == 0 {
            return None;
        }
        Some((id, PhysicalPosition::new(f64::from(point.x), f64::from(point.y))))
    }

    pub(super) fn set_local_drag_cursor(&self, window: HWND) -> bool {
        if let Some(drag) =
            self.local_drag.borrow().as_ref().filter(|drag| drag.source.into_raw() == window.addr())
        {
            set_cursor(drag.cursor());
            return true;
        }
        false
    }

    pub(super) fn release_local_drag(self: &Rc<Self>, window: HWND, position: u32, keys: u32) {
        if keys & MOUSE_BUTTONS != 0 {
            return;
        }
        let x = i32::from(util::get_x_lparam(position));
        let y = i32::from(util::get_y_lparam(position));
        if self.local_drag_motion(window, x, y, keys) {
            self.finish_local_drag(WindowId::from_raw(window.addr()), true);
        }
    }

    pub(super) fn cancel_local_drag(self: &Rc<Self>, window: HWND) {
        self.finish_local_drag(WindowId::from_raw(window.addr()), false);
    }

    pub(super) fn local_drag_window_destroyed(self: &Rc<Self>, window: WindowId) {
        self.finish_local_drag(window, false);
        let mut local = self.local_drag.borrow_mut();
        if let Some(drag) = local.as_mut().filter(|drag| drag.target == Some(window)) {
            drag.target = None;
            self.data_transfer().remove_drag(drag.id);
            drag.effect = DROPEFFECT_NONE;
            set_cursor(drag.cursor());
        }
    }

    fn finish_local_drag(self: &Rc<Self>, source: WindowId, release: bool) {
        let drag = self.local_drag.borrow_mut().take_if(|drag| drag.source == source);
        let Some(drag) = drag else { return };
        set_cursor(drag.default_cursor);
        let action = release.then(|| drop_effect_to_dnd_action(drag.effect)).flatten();
        let outcome = match (drag.target, action) {
            (Some(target), Some(action)) => {
                self.defer_source_drag_cleanup(drag.id);
                self.send_local_window_event(target, WindowEvent::DragDropped {
                    id: drag.id,
                    proposed_action: Some(action),
                });
                WindowEvent::OutgoingDragDropped { id: drag.id, action: Some(action) }
            },
            (target, _) => {
                self.data_transfer().remove_drag(drag.id);
                if let Some(target) = target {
                    self.send_local_window_event(target, WindowEvent::DragLeft { id: drag.id });
                }
                WindowEvent::OutgoingDragCanceled { id: drag.id }
            },
        };
        self.send_local_window_event(source, outcome);
    }

    fn send_local_window_event(self: &Rc<Self>, window_id: WindowId, event: WindowEvent) {
        self.send_event(Event::Window { window_id, event });
    }
}

fn set_cursor(cursor: HCURSOR) {
    if !cursor.is_null() {
        // SAFETY: The named handles are shared or owned by the cursor cache.
        unsafe { SetCursor(cursor) };
    }
}
