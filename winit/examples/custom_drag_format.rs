//! Drag from the source client area to the target. Press N in the source to
//! toggle an unrecognized format. Press Escape during the drag to cancel.

use std::error::Error;

use winit::application::ApplicationHandler;
use winit::data_transfer::{CustomDragFormat, DataTransferId, DataTransferSendBuilder};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ButtonSource, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, AsyncRequestSerial, DndAction, EventLoop};
use winit::keyboard::Key;
use winit::window::{Window, WindowAttributes, WindowId};

#[path = "util/tracing.rs"]
mod tracing;

fn main() -> Result<(), Box<dyn Error>> {
    tracing::init();
    let app = Example {
        format: CustomDragFormat::new("org.example.drag-example")?,
        unknown: CustomDragFormat::new("org.example.unknown-drag-example")?,
        send_unknown: false,
        windows: None,
        press: None,
        sequence: 0,
        pending: None,
    };
    Ok(EventLoop::new()?.run_app(app)?)
}

struct Example {
    format: CustomDragFormat,
    unknown: CustomDragFormat,
    send_unknown: bool,
    windows: Option<(Box<dyn Window>, Box<dyn Window>)>,
    press: Option<PhysicalPosition<f64>>,
    sequence: u64,
    pending: Option<(DataTransferId, AsyncRequestSerial)>,
}

impl ApplicationHandler for Example {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.windows.is_some() {
            return;
        }
        let source = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Drag here to start a custom drag")
                    .with_position(PhysicalPosition::new(100, 120))
                    .with_surface_size(PhysicalSize::new(300, 160)),
            )
            .expect("create source window");
        let target = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Drop the custom format here")
                    .with_position(PhysicalPosition::new(450, 120))
                    .with_surface_size(PhysicalSize::new(300, 160))
                    .with_custom_drag_formats([self.format.clone()]),
            )
            .expect("create target window");
        self.windows = Some((source, target));
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window: WindowId,
        event: WindowEvent,
    ) {
        let Some((source, target)) = &self.windows else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if window == source.id() && event.state.is_pressed() =>
            {
                if matches!(event.logical_key, Key::Character(c) if c == "n") {
                    self.send_unknown = !self.send_unknown;
                    tracing::info!("Offering an unrecognized format: {}", self.send_unknown);
                }
            },
            WindowEvent::PointerButton {
                button: ButtonSource::Mouse(MouseButton::Left),
                state,
                position,
                ..
            } if window == source.id() => {
                self.press = state.is_pressed().then_some(position);
            },
            WindowEvent::PointerMoved { position, .. } if window == source.id() => {
                if self
                    .press
                    .is_some_and(|start| (start.x - position.x).hypot(start.y - position.y) >= 5.0)
                {
                    self.press = None;
                    self.sequence += 1;
                    let mut bytes = self.sequence.to_le_bytes().to_vec();
                    bytes.push(0);
                    let format = if self.send_unknown { &self.unknown } else { &self.format };
                    let offer = DataTransferSendBuilder::new(bytes)
                        .with_type(format.clone(), |bytes, _| Some(bytes.clone()))
                        .build();
                    if let Err(error) =
                        event_loop.start_drag(window, offer, &[DndAction::Copy], None)
                    {
                        tracing::error!("Could not start drag: {error}");
                    }
                }
            },
            WindowEvent::DragEntered { id, .. } if window == target.id() => {
                let offered =
                    event_loop.data_transfer(id).is_ok_and(|offer| offer.has_type(&self.format));
                let actions: &[DndAction] = if offered { &[DndAction::Copy] } else { &[] };
                if let Err(error) = event_loop.set_valid_dnd_actions(id, actions) {
                    tracing::error!("Could not set drop action: {error}");
                }
            },
            WindowEvent::DragDropped { id, .. } if window == target.id() => {
                match event_loop.fetch_data_transfer(id, &self.format) {
                    Ok(serial) => self.pending = Some((id, serial)),
                    Err(error) => tracing::error!("Could not fetch custom bytes: {error}"),
                }
            },
            WindowEvent::DragLeft { id }
                if self.pending.is_some_and(|(pending, _)| pending == id) =>
            {
                self.pending = None;
            },
            WindowEvent::DataTransferReceived { id, serial, value }
                if window == target.id() && self.pending == Some((id, serial)) =>
            {
                self.pending = None;
                match value.try_as_bytes() {
                    Ok(bytes) => {
                        let message = format!("Received {} bytes: {bytes:?}", bytes.len());
                        tracing::info!("{message}");
                        target.set_title(&message);
                    },
                    Err(error) => tracing::error!("Could not read custom bytes: {error}"),
                }
            },
            WindowEvent::OutgoingDragCanceled { .. } => tracing::info!("Drag canceled"),
            _ => (),
        }
    }
}
