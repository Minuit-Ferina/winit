import AppKit
import CoreGraphics

func require(_ condition: Bool, _ message: String) {
    if !condition {
        FileHandle.standardError.write(Data((message + "\n").utf8))
        exit(1)
    }
}

require(CGPreflightPostEventAccess(), "This runner cannot post synthetic desktop input.")
if CommandLine.arguments == [CommandLine.arguments[0], "--preflight"] {
    exit(0)
}
require(CommandLine.arguments.count == 3, "Expected the demo and target process IDs")
let demo = Int(CommandLine.arguments[1])!
let external = Int(CommandLine.arguments[2])!

func windows(of pid: Int) -> [CGRect] {
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    return list.compactMap { window in
        guard window[kCGWindowOwnerPID as String] as? Int == pid,
              window[kCGWindowLayer as String] as? Int == 0,
              let bounds = window[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds as CFDictionary),
              rect.width > 100 && rect.height > 80 else { return nil }
        return rect
    }.sorted { $0.minX < $1.minX }
}

for _ in 0..<60 {
    if windows(of: demo).count == 2 && windows(of: external).count == 1 { break }
    usleep(150_000)
}

let appWindows = windows(of: demo)
let externalWindows = windows(of: external)
require(appWindows.count == 2 && externalWindows.count == 1, "Could not locate all three native windows")
func center(_ rect: CGRect) -> CGPoint { CGPoint(x: rect.midX, y: rect.midY + 12) }
let source = center(appWindows[0])
let target = center(appWindows[1])
let outside = center(externalWindows[0])
print("Source: \(source), target: \(target), external: \(outside)")
require(appWindows[1].contains(target) && !externalWindows[0].contains(target), "Overlapping target windows")

func mouse(_ kind: CGEventType, _ at: CGPoint) {
    CGEvent(mouseEventSource: nil, mouseType: kind, mouseCursorPosition: at, mouseButton: .left)!
        .post(tap: .cghidEventTap)
    usleep(35_000)
}

func drag(to point: CGPoint, escape: Bool = false) {
    mouse(.mouseMoved, source)
    mouse(.leftMouseDown, source)
    mouse(.leftMouseUp, source)
    usleep(120_000)
    mouse(.leftMouseDown, source)
    for step in 1...24 {
        let fraction = CGFloat(step) / 24
        mouse(.leftMouseDragged, CGPoint(
            x: source.x + fraction * (point.x - source.x),
            y: source.y + fraction * (point.y - source.y)
        ))
    }
    usleep(250_000)
    if escape {
        for down in [true, false] {
            CGEvent(keyboardEventSource: nil, virtualKey: 53, keyDown: down)!
                .post(tap: .cghidEventTap)
            usleep(70_000)
        }
    }
    mouse(.leftMouseUp, point)
    usleep(400_000)
}

drag(to: target)
drag(to: outside)
drag(to: target, escape: true)
drag(to: target)
