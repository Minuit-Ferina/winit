//! Drag from the source client area to the target. Press N in the source to
//! toggle an unrecognized format. Press Escape during the drag to cancel.

use std::error::Error;

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::data_transfer::{CustomDragFormat, DataTransferId, DataTransferSendBuilder, TypeHint};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ButtonSource, MouseButton, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, AsyncRequestSerial, DndAction, EventLoop, OwnedDisplayHandle,
};
use winit::keyboard::Key;
use winit::window::{Window, WindowAttributes, WindowId};

#[path = "util/fill.rs"]
mod fill;
#[path = "util/tracing.rs"]
mod tracing;

fn main() -> Result<(), Box<dyn Error>> {
    tracing::init();
    if let Some(mode) = std::env::args().nth(1) {
        return Ok(EventLoop::new()?
            .run_app(Foreign { surface: None, accept_marker: mode == "accept-marker" })?);
    }
    let app = Example {
        format: CustomDragFormat::new("org.example.drag-example")?,
        unknown: CustomDragFormat::new("org.example.unknown-drag-example")?,
        send_unknown: false,
        surfaces: None,
        press: None,
        sequence: 0,
        pending: None,
    };
    Ok(EventLoop::new()?.run_app(app)?)
}

type TestSurface = Surface<OwnedDisplayHandle, Box<dyn Window>>;

struct Example {
    format: CustomDragFormat,
    unknown: CustomDragFormat,
    send_unknown: bool,
    surfaces: Option<(TestSurface, TestSurface)>,
    press: Option<PhysicalPosition<f64>>,
    sequence: u64,
    pending: Option<(DataTransferId, AsyncRequestSerial)>,
}

impl ApplicationHandler for Example {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.surfaces.is_some() {
            return;
        }
        let source = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Drag here to start a custom drag")
                    .with_position(PhysicalPosition::new(100, 120))
                    .with_surface_size(PhysicalSize::new(420, 330)),
            )
            .expect("create source window");
        let target = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Drop the custom format here")
                    .with_position(PhysicalPosition::new(450, 120))
                    .with_surface_size(PhysicalSize::new(340, 250))
                    .with_custom_drag_formats([self.format.clone()]),
            )
            .expect("create target window");
        let context = Context::new(event_loop.owned_display_handle()).expect("create context");
        self.surfaces = Some((
            Surface::new(&context, source).expect("create source surface"),
            Surface::new(&context, target).expect("create target surface"),
        ));
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window: WindowId,
        event: WindowEvent,
    ) {
        let Some((source, target)) = &mut self.surfaces else { return };
        if matches!(event, WindowEvent::RedrawRequested) {
            let (surface, color) = if window == source.window().id() {
                (source, 0x00eb402a)
            } else {
                (target, 0x002c57e8)
            };
            surface.window().pre_present_notify();
            fill::fill_with_color(surface, color);
            return;
        }
        let (source, target) = (source.window(), target.window());
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
                        .with_type(TypeHint::Plaintext, |_, _| {
                            Some("In-process test text".to_owned())
                        })
                        .build();
                    if let Err(error) =
                        event_loop.start_internal_drag(window, offer, &[DndAction::Copy], None)
                    {
                        panic!("Could not start drag: {error}");
                    }
                    println!("START {}", self.sequence);
                }
            },
            WindowEvent::DragEntered { id, .. } if window == target.id() => {
                let offered =
                    event_loop.data_transfer(id).is_ok_and(|offer| offer.has_type(&self.format));
                println!("TARGET_ENTER {offered}");
                let actions: &[DndAction] = if offered { &[DndAction::Copy] } else { &[] };
                if let Err(error) = event_loop.set_valid_dnd_actions(id, actions) {
                    tracing::error!("Could not set drop action: {error}");
                }
            },
            WindowEvent::DragDropped { id, .. } if window == target.id() => {
                println!("TARGET_DROP");
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
                        println!("RECEIVED {bytes:?}");
                        target.set_title(&message);
                    },
                    Err(error) => tracing::error!("Could not read custom bytes: {error}"),
                }
            },
            WindowEvent::OutgoingDragCanceled { .. } => println!("CANCEL"),
            WindowEvent::OutgoingDragDropped { action, .. } => println!("DROPPED {action:?}"),
            _ => (),
        }
    }
}

struct Foreign {
    surface: Option<TestSurface>,
    accept_marker: bool,
}

impl ApplicationHandler for Foreign {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Foreign text destination")
                    .with_surface_size(PhysicalSize::new(260, 190)),
            )
            .expect("create foreign window");
        let context = Context::new(event_loop.owned_display_handle()).expect("create context");
        self.surface = Some(Surface::new(&context, window).expect("create foreign surface"));
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(surface) = &mut self.surface else { return };
        match event {
            WindowEvent::RedrawRequested => {
                surface.window().pre_present_notify();
                fill::fill_with_color(surface, 0x002ac950);
            },
            WindowEvent::DragEntered { id, .. } => {
                let offer = event_loop.data_transfer(id).expect("inspect foreign offer");
                assert!(!offer.has_type(&TypeHint::Plaintext));
                assert!(offer.available_types().iter().all(|ty| ty.custom_drag_format().is_none()));
                let actions: &[_] = if self.accept_marker { &[DndAction::Copy] } else { &[] };
                event_loop.set_valid_dnd_actions(id, actions).expect("set foreign drop actions");
                println!("FOREIGN_ENTER {}", self.accept_marker);
            },
            WindowEvent::DragDropped { id, .. } => {
                assert!(self.accept_marker);
                let offer = event_loop.data_transfer(id).expect("inspect foreign offer");
                let marker = offer.available_types()[0];
                event_loop.fetch_data_transfer(id, marker).expect("request marker");
                println!("FOREIGN_DROP");
            },
            WindowEvent::DataTransferReceived { value, .. } => {
                assert!(value.try_as_bytes().is_ok_and(|bytes| bytes.is_empty()));
                println!("FOREIGN_EMPTY");
            },
            _ => (),
        }
    }
}
