//! Run under a disposable X server with xdotool installed:
//! xvfb-run -a cargo test -p winit-x11 --test outgoing_drag -- --ignored

use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dpi::{PhysicalPosition, PhysicalSize};
use winit_core::application::ApplicationHandler;
use winit_core::data_transfer::{DataTransferId, DataTransferSendBuilder, TypeHint};
use winit_core::event::{ButtonSource, ElementState, MouseButton, WindowEvent};
use winit_core::event_loop::{ActiveEventLoop, ControlFlow, DndAction};
use winit_core::window::{Window, WindowAttributes, WindowId};

const PAYLOAD: &str = "winit XDND source";

#[test]
#[ignore = "requires a disposable X11 display and xdotool"]
fn completes_and_cancels_drags_between_x11_windows() {
    let mut events = winit_x11::EventLoop::new().unwrap();
    for scenario in [Scenario::Copy, Scenario::Reject, Scenario::Escape, Scenario::Copy] {
        let mut app = DragTest {
            scenario,
            source: None,
            destination: None,
            pointer_down: false,
            transfer: None,
            received: None,
            outcome: None,
            visited_destination: false,
            driver: None,
            deadline: Instant::now() + Duration::from_secs(12),
        };
        events.run_app_on_demand(&mut app).unwrap();
        app.driver.take().unwrap().join().unwrap();
        assert!(app.visited_destination, "{scenario:?}");
        let copied = scenario == Scenario::Copy;
        assert_eq!(app.received.as_deref(), copied.then_some(PAYLOAD), "{scenario:?}");
        assert_eq!(app.outcome, Some(copied), "{scenario:?}");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Copy,
    Reject,
    Escape,
}

struct DragTest {
    scenario: Scenario,
    source: Option<Box<dyn Window>>,
    destination: Option<Box<dyn Window>>,
    pointer_down: bool,
    transfer: Option<DataTransferId>,
    received: Option<String>,
    outcome: Option<bool>,
    visited_destination: bool,
    driver: Option<JoinHandle<()>>,
    deadline: Instant,
}

impl ApplicationHandler for DragTest {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attributes = WindowAttributes::default()
            .with_decorations(false)
            .with_surface_size(PhysicalSize::new(180, 180));
        let source = event_loop
            .create_window(
                attributes
                    .clone()
                    .with_title("winit-x11-source")
                    .with_position(PhysicalPosition::new(30, 30)),
            )
            .unwrap();
        let destination = event_loop
            .create_window(
                attributes
                    .with_title("winit-x11-target")
                    .with_position(PhysicalPosition::new(330, 30)),
            )
            .unwrap();
        let source_xid = source.id().into_raw().to_string();
        self.source = Some(source);
        self.destination = Some(destination);

        let scenario = self.scenario;
        self.driver = Some(thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let mut input = Command::new("xdotool");
            input.args([
                "windowfocus",
                &source_xid,
                "mousemove",
                "95",
                "90",
                "mousedown",
                "1",
                "sleep",
                "0.05",
                "mousemove",
                "110",
                "90",
                "sleep",
                "0.1",
                "mousemove",
                "400",
                "90",
                "sleep",
                "0.2",
            ]);
            if scenario == Scenario::Escape {
                input.args(["key", "Escape", "sleep", "0.05"]);
            }
            assert!(input.args(["mouseup", "1"]).status().unwrap().success());
        }));
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window: WindowId,
        event: WindowEvent,
    ) {
        let source = self.source.as_ref().is_some_and(|source| source.id() == window);
        let destination =
            self.destination.as_ref().is_some_and(|destination| destination.id() == window);

        match event {
            WindowEvent::PointerButton {
                button: ButtonSource::Mouse(MouseButton::Left),
                state: ElementState::Pressed,
                ..
            } if source => {
                self.pointer_down = true;
            },
            WindowEvent::PointerMoved { position, .. }
                if source
                    && self.pointer_down
                    && self.transfer.is_none()
                    && position.x >= 180.0 =>
            {
                let data = DataTransferSendBuilder::new(PAYLOAD)
                    .with_type(TypeHint::Plaintext, |value, _| Some(value.to_string()))
                    .build();
                self.transfer =
                    Some(event_loop.start_drag(window, data, &[DndAction::Copy], None).unwrap());
            },
            WindowEvent::DragPosition { id, .. } if destination => {
                self.visited_destination = true;
                let actions = if self.scenario == Scenario::Reject {
                    &[][..]
                } else {
                    &[DndAction::Copy][..]
                };
                event_loop.set_valid_dnd_actions(id, actions).unwrap();
            },
            WindowEvent::DragDropped { id, .. } if destination => {
                assert_eq!(self.scenario, Scenario::Copy);
                event_loop.fetch_data_transfer(id, &TypeHint::Plaintext).unwrap();
            },
            WindowEvent::DataTransferReceived { value, .. } if destination => {
                self.received = Some(value.try_as_string().unwrap());
            },
            WindowEvent::OutgoingDragDropped { id, action } if source => {
                assert_eq!(Some(id), self.transfer);
                assert_eq!(action, Some(DndAction::Copy));
                assert_eq!(self.outcome.replace(true), None);
            },
            WindowEvent::OutgoingDragCanceled { id } if source => {
                assert_eq!(Some(id), self.transfer);
                assert_eq!(self.outcome.replace(false), None);
            },
            _ => (),
        }
        if self.outcome == Some(false) || self.outcome == Some(true) && self.received.is_some() {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if Instant::now() >= self.deadline {
            event_loop.exit();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }
    }
}
