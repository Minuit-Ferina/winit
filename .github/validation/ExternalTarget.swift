import AppKit

final class ExternalTarget: NSView {
    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        return .copy
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        print("Foreign drop performed")
        fflush(stdout)
        return true
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let screen = NSScreen.main!
let window = NSWindow(
    contentRect: NSRect(x: 0, y: 0, width: 160, height: 130),
    styleMask: [.titled],
    backing: .buffered,
    defer: false
)
window.title = "External test destination"
window.setFrameTopLeftPoint(NSPoint(x: min(800, screen.frame.maxX - 180), y: screen.frame.maxY - 140))
let target = ExternalTarget(frame: window.contentView!.bounds)
window.contentView = target
target.registerForDraggedTypes([NSPasteboard.PasteboardType("org.rust-windowing.winit.drag.org.example.drag-example")])
window.orderFront(nil)
app.run()
