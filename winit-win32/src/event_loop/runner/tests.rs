use std::cell::RefCell;
use std::rc::Rc;
use std::{panic, ptr};

use windows_sys::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, E_FAIL};
use windows_sys::Win32::System::Ole::DROPEFFECT_COPY;
use winit_core::application::ApplicationHandler;
use winit_core::data_transfer::{DataTransferId, DataTransferSendBuilder, TypeHint};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{ActiveEventLoop as RootActiveEventLoop, DndAction};
use winit_core::window::WindowId;

use super::{ActiveEventLoop, Event, EventLoopRunner};

#[derive(Default)]
struct Recorder(Rc<RefCell<Vec<(WindowId, WindowEvent)>>>);

impl ApplicationHandler for Recorder {
    fn can_create_surfaces(&mut self, _: &dyn RootActiveEventLoop) {}

    fn window_event(&mut self, _: &dyn RootActiveEventLoop, window: WindowId, event: WindowEvent) {
        self.0.borrow_mut().push((window, event));
    }
}

fn with_runner(test: impl FnOnce(&Rc<EventLoopRunner>, &Recorder)) {
    let recorder = Recorder::default();
    let mut app = Recorder(Rc::clone(&recorder.0));
    let runner = Rc::new(EventLoopRunner::new(0, ptr::null_mut()));
    // SAFETY: app outlives the resetter, which stays on this stack until the test has finished.
    let _resetter = unsafe { runner.set_app(&mut app) };
    test(&runner, &recorder);
    runner.take_panic_error().expect("application callback did not panic");
}

fn queue_drag(event_loop: &dyn RootActiveEventLoop) -> DataTransferId {
    event_loop
        .start_drag(
            WindowId::from_raw(7),
            DataTransferSendBuilder::new(())
                .with_type(TypeHint::Plaintext, |_, _| Some("test drag".to_owned()))
                .build(),
            &[DndAction::Copy],
            None,
        )
        .expect("a new drag can be queued")
}

#[test]
fn nested_dispatch_defers_the_drag_until_application_callbacks_can_run() {
    with_runner(|runner, recorder| {
        runner.call_event_handler(|_, event_loop| {
            let id = queue_drag(event_loop);
            assert!(
                event_loop
                    .start_drag(
                        WindowId::from_raw(8),
                        DataTransferSendBuilder::new(()).build(),
                        &[DndAction::Copy],
                        None,
                    )
                    .is_err()
            );
            runner.try_execute_drag_drop_with(|_, _| {
                panic!("a nested window callback must not enter the native drag loop")
            });
            assert_eq!(runner.pending_drag.borrow().as_ref().map(|drag| drag.id), Some(id));
        });
        runner.take_panic_error().expect("nested callback kept the drag pending");

        let id = runner.pending_drag.borrow().as_ref().unwrap().id;
        runner.try_execute_drag_drop_with(|pending, _| {
            assert_eq!(pending.id, id);
            assert!(!runner.should_buffer());
            runner.send_event(Event::Window {
                window_id: pending.window_id,
                event: WindowEvent::RedrawRequested,
            });
            assert!(matches!(recorder.0.borrow().as_slice(), [(_, WindowEvent::RedrawRequested)]));

            // Reentrant dispatch must neither replace this drag nor start another native loop.
            assert!(
                ActiveEventLoop::from_ref(runner)
                    .start_drag(
                        pending.window_id,
                        DataTransferSendBuilder::new(()).build(),
                        &[DndAction::Copy],
                        None,
                    )
                    .is_err()
            );
            runner.try_execute_drag_drop_with(|_, _| panic!("a drag is already active"));
            DRAGDROP_S_CANCEL
        });

        let events = recorder.0.borrow();
        assert!(
            matches!(events.as_slice(), [(_, WindowEvent::RedrawRequested), (_, WindowEvent::OutgoingDragCanceled { id: canceled })] if *canceled == id)
        );
        assert!(runner.pending_drag.borrow().is_none());
        assert!(runner.source_drag.get().is_none());
    });
}

#[test]
fn native_drag_outcomes_clear_the_source_and_allow_the_next_drag() {
    with_runner(|runner, recorder| {
        for result in [DRAGDROP_S_DROP, DRAGDROP_S_CANCEL, E_FAIL] {
            let id = queue_drag(ActiveEventLoop::from_ref(runner));
            runner.try_execute_drag_drop_with(|_, effect| {
                *effect = DROPEFFECT_COPY;
                result
            });
            let mut events = recorder.0.borrow_mut();
            assert_eq!(events.len(), 1, "one terminal event per native drag");
            let (window, event) = events.pop().unwrap();
            assert_eq!(window, WindowId::from_raw(7));
            if result == DRAGDROP_S_DROP {
                assert!(
                    matches!(event, WindowEvent::OutgoingDragDropped { id: dropped, action: Some(DndAction::Copy) } if dropped == id)
                );
            } else {
                assert!(
                    matches!(event, WindowEvent::OutgoingDragCanceled { id: canceled } if canceled == id)
                );
            }
            assert!(runner.pending_drag.borrow().is_none());
            assert!(runner.source_drag.get().is_none());
        }
    });
}

#[test]
fn unwinding_the_native_drag_does_not_leave_an_active_source() {
    with_runner(|runner, _| {
        queue_drag(ActiveEventLoop::from_ref(runner));
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            runner.try_execute_drag_drop_with(|_, _| panic!("native-call test seam"));
        }));
        assert!(result.is_err());
        assert!(runner.pending_drag.borrow().is_none());
        assert!(runner.source_drag.get().is_none());
        queue_drag(ActiveEventLoop::from_ref(runner));
        runner.try_execute_drag_drop_with(|_, _| DRAGDROP_S_CANCEL);
    });
}
