from pathlib import Path
import subprocess
import sys
import time

from PIL import ImageGrab


log_dir, demo = Path(sys.argv[1]), sys.argv[2]
app_log = log_dir / "app.log"
foreign_log = log_dir / "foreign.log"
source_color = (235, 64, 42)
target_color = (44, 87, 232)
outside_color = (42, 201, 80)


def launch(log, *args):
    with log.open("w") as output:
        return subprocess.Popen([demo, *args], stdout=output, stderr=subprocess.STDOUT)


def points(*colors):
    for _ in range(30):
        screen = ImageGrab.grab()
        width, height = screen.size
        found = {color: [] for color in colors}
        for y in range(12, height - 12, 8):
            for x in range(12, width - 12, 8):
                color = screen.getpixel((x, y))
                if color in found and all(
                    screen.getpixel((x + dx, y + dy)) == color
                    for dx, dy in [(-10, 0), (10, 0), (0, -10), (0, 10)]
                ):
                    found[color].append((x, y))
        if all(found.values()):
            return [found[color][len(found[color]) // 2] for color in colors]
        time.sleep(0.2)
    screen.save(log_dir / "desktop.png")
    raise AssertionError(f"Missing window colors: {list(found)}")


def input_(*args):
    subprocess.run(["xdotool", *map(str, args)], check=True)
    time.sleep(0.20)


def wait_for(log, marker, count=1):
    for _ in range(45):
        lines = log.read_text()
        if lines.count(marker) >= count:
            return
        time.sleep(0.1)
    raise AssertionError(f"{log.name}: expected {count} occurrences of {marker!r}\\n{lines}")


def begin(destination, number):
    source, dest = points(source_color, destination)
    input_("mousemove", *source, "click", 1)
    source, dest = points(source_color, destination)
    input_("mousemove", *source, "mousedown", 1)
    input_("mousemove", source[0] + 8, source[1])
    wait_for(app_log, f"START {number}")
    input_("mousemove", *dest)


app = launch(app_log)
foreign = None
try:
    begin(target_color, 1)
    wait_for(app_log, "TARGET_ENTER true")
    input_("mouseup", 1)
    wait_for(app_log, "DROPPED Copy")
    wait_for(app_log, "RECEIVED [1, 0, 0, 0, 0, 0, 0, 0, 0]")

    foreign = launch(foreign_log, "text")
    begin(outside_color, 2)
    wait_for(foreign_log, "FOREIGN_ENTER false")
    input_("mouseup", 1)
    wait_for(app_log, "CANCEL")
    assert "FOREIGN_DROP" not in foreign_log.read_text()
    foreign.terminate()
    foreign.wait(timeout=5)
    foreign = None

    begin(target_color, 3)
    wait_for(app_log, "TARGET_ENTER true", 2)
    input_("key", "Escape")
    wait_for(app_log, "CANCEL", 2)
    input_("mouseup", 1)

    begin(target_color, 4)
    wait_for(app_log, "TARGET_ENTER true", 3)
    input_("mouseup", 1)
    wait_for(app_log, "RECEIVED [4, 0, 0, 0, 0, 0, 0, 0, 0]")

    foreign = launch(foreign_log, "accept-marker")
    begin(outside_color, 5)
    wait_for(foreign_log, "FOREIGN_ENTER true")
    input_("mouseup", 1)
    wait_for(app_log, "CANCEL", 3)
    wait_for(foreign_log, "FOREIGN_EMPTY")

    lines = app_log.read_text().splitlines()
    assert lines.count("DROPPED Copy") == 2, lines
    assert lines.count("TARGET_DROP") == 2, lines
    assert lines.count("CANCEL") == 3, lines
    print("Passed: internal copy, external refusal, Escape, copy after Escape, marker without payload")
finally:
    (log_dir / "desktop.png").exists() or ImageGrab.grab().save(log_dir / "desktop.png")
    app.terminate()
    if foreign is not None:
        foreign.terminate()
    for log in (app_log, foreign_log):
        if log.exists():
            print(log.read_text())
