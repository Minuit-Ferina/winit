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
use winit_x11::ActiveEventLoopExtX11 as _;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, ConnectionExt as _};

const PAYLOAD: &str = "winit XDND source";

#[test]
#[ignore = "requires a disposable X11 display and xdotool"]
fn completes_and_cancels_drags_between_x11_windows() {
    let mut events = winit_x11::EventLoop::new().unwrap();
    for scenario in [
        Scenario::Copy,
        Scenario::CopyAcrossGap,
        Scenario::CopyAcrossOverlap,
        Scenario::CopyWithinSource,
        Scenario::Reject,
        Scenario::Escape,
        Scenario::Copy,
    ] {
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
        let copied = scenario.copies();
        assert_eq!(app.received.as_deref(), copied.then_some(PAYLOAD), "{scenario:?}");
        assert_eq!(app.outcome, Some(copied), "{scenario:?}");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Copy,
    CopyAcrossGap,
    CopyAcrossOverlap,
    CopyWithinSource,
    Reject,
    Escape,
}

impl Scenario {
    fn copies(self) -> bool {
        matches!(
            self,
            Self::Copy | Self::CopyAcrossGap | Self::CopyAcrossOverlap | Self::CopyWithinSource
        )
    }

    fn target_x(self) -> i32 {
        match self {
            Self::CopyAcrossGap => 214,
            Self::CopyAcrossOverlap => 180,
            _ => 330,
        }
    }
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
        let destination = event_loop
            .create_window(
                attributes
                    .clone()
                    .with_title("winit-x11-target")
                    .with_position(PhysicalPosition::new(self.scenario.target_x(), 30)),
            )
            .unwrap();
        let source = event_loop
            .create_window(
                attributes
                    .with_title("winit-x11-source")
                    .with_position(PhysicalPosition::new(30, 30)),
            )
            .unwrap();
        let source_xid = source.id().into_raw().to_string();
        let destination_xid = destination.id().into_raw().to_string();
        self.source = Some(source);
        self.destination = Some(destination);

        let scenario = self.scenario;
        self.driver = Some(thread::spawn(move || {
            let windows_visible = (0..80).any(|_| {
                if Command::new("xdotool")
                    .args(["search", "--onlyvisible", "--name", "^winit-x11-(source|target)$"])
                    .output()
                    .is_ok_and(|output| {
                        String::from_utf8_lossy(&output.stdout).lines().count() == 2
                    })
                {
                    return true;
                }
                thread::sleep(Duration::from_millis(25));
                false
            });
            assert!(windows_visible, "both test windows should be visible");
            let mut input = Command::new("xdotool");
            let target_x = scenario.target_x().to_string();
            input.args([
                "windowmove",
                &destination_xid,
                &target_x,
                "30",
                "windowmove",
                &source_xid,
                "30",
                "30",
                "sleep",
                "0.15",
                "windowraise",
                &source_xid,
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
                match scenario {
                    Scenario::CopyAcrossGap | Scenario::CopyAcrossOverlap => "222",
                    Scenario::CopyWithinSource => "150",
                    _ => "400",
                },
                "90",
                "sleep",
                "0.2",
            ]);
            if matches!(scenario, Scenario::CopyAcrossGap | Scenario::CopyAcrossOverlap) {
                // Revisit the source briefly, then cross straight back into the target.
                input.args([
                    "mousemove",
                    "206",
                    "90",
                    "sleep",
                    "0.03",
                    "mousemove",
                    "222",
                    "90",
                    "sleep",
                    "0.1",
                ]);
            }
            if scenario == Scenario::Escape {
                input.args(["key", "Escape", "sleep", "0.05"]);
            }
            assert!(input.status().unwrap().success());
            if scenario == Scenario::CopyAcrossGap {
                check_selection_during_hover();
            }
            assert!(Command::new("xdotool").args(["mouseup", "1"]).status().unwrap().success());
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

        let target = if self.scenario == Scenario::CopyWithinSource { source } else { destination };
        let start_x = if event_loop.is_xwayland() || self.scenario == Scenario::CopyWithinSource {
            75.0
        } else {
            180.0
        };

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
                    && position.x >= start_x =>
            {
                let data = DataTransferSendBuilder::new(PAYLOAD)
                    .with_type(TypeHint::Plaintext, |value, _| Some(value.to_string()))
                    .build();
                self.transfer =
                    Some(event_loop.start_drag(window, data, &[DndAction::Copy], None).unwrap());
            },
            WindowEvent::DragPosition { id, .. } if target => {
                self.visited_destination = true;
                let actions = if self.scenario == Scenario::Reject {
                    &[][..]
                } else {
                    &[DndAction::Copy][..]
                };
                event_loop.set_valid_dnd_actions(id, actions).unwrap();
            },
            WindowEvent::DragDropped { id, .. } if target => {
                assert!(self.scenario.copies());
                event_loop.fetch_data_transfer(id, &TypeHint::Plaintext).unwrap();
            },
            WindowEvent::DataTransferReceived { value, .. } if target => {
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

fn check_selection_during_hover() {
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &xproto::CreateWindowAux::new(),
    )
    .unwrap()
    .check()
    .unwrap();

    let atom = |name| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
    let selection = atom(b"XdndSelection");
    let targets = atom(b"TARGETS");
    let text = atom(b"UTF8_STRING");
    let property = atom(b"WINIT_TEST_SELECTION");
    for (target, expected_property) in [(targets, property), (text, x11rb::NONE)] {
        conn.convert_selection(window, selection, target, property, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        conn.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(Event::SelectionNotify(reply)) = conn.poll_for_event().unwrap() {
                assert_eq!(reply.target, target);
                assert_eq!(reply.property, expected_property);
                break;
            }
            assert!(Instant::now() < deadline, "missing XdndSelection reply");
            thread::sleep(Duration::from_millis(5));
        }
        if target == targets {
            let reply = conn
                .get_property(true, window, property, xproto::AtomEnum::ATOM, 0, 100)
                .unwrap()
                .reply()
                .unwrap();
            assert!(reply.value32().unwrap().any(|format| format == text));
        }
    }
}
