"""FEMME on a real iPhone in AWS Device Farm (Appium 3, XCUITest): pairs with the live box by
typing the host and token from tests/pair.txt (a femme://pair link) into the pairing screen and
tapping Connect, then walks the tabs and opens a chat, screenshotting each step into
$DEVICEFARM_SCREENSHOT_PATH. Run by mobile/ci/devicefarm-ios.sh with Device Farm's default
"iOS Test Spec for Appium 3 Python", which starts Appium with the device and app as defaults.

Flutter rebuilds its accessibility tree whenever focus changes, so an element found before a
click is stale after it: look elements up fresh each time and type into the active element."""

import os
import pathlib
import time
import urllib.parse as up

from appium import webdriver
from appium.options.ios import XCUITestOptions
from appium.webdriver.common.appiumby import AppiumBy
from selenium.common.exceptions import StaleElementReferenceException

HERE = pathlib.Path(__file__).parent
SHOTS = pathlib.Path(os.environ.get("DEVICEFARM_SCREENSHOT_PATH", "shots"))
LOG = pathlib.Path(os.environ.get("DEVICEFARM_LOG_DIR", "."))


def note(msg):
    print(msg, flush=True)
    with open(LOG / "femme-test.txt", "a") as f:
        f.write(msg + "\n")


def test_femme_on_iphone():
    SHOTS.mkdir(parents=True, exist_ok=True)
    link = (HERE / "pair.txt").read_text().strip()
    q = up.parse_qs(up.urlparse(link).query)
    host = ("wss://" if q.get("tls", ["1"])[0] == "1" else "ws://") + q["host"][0]
    token = q["token"][0]

    opts = XCUITestOptions()
    opts.platform_name = "iOS"
    opts.automation_name = "XCUITest"
    opts.set_capability("newCommandTimeout", 300)
    opts.set_capability("wdaLaunchTimeout", 120000)
    opts.set_capability("autoAcceptAlerts", True)
    d = webdriver.Remote(os.environ.get("APPIUM_URL", "http://127.0.0.1:4723"), options=opts)
    try:
        size = d.get_window_size()
        W, H = size["width"], size["height"]
        note(f"window {W}x{H} pt, iOS {os.environ.get('DEVICEFARM_DEVICE_OS_VERSION')}")
        time.sleep(8)
        d.get_screenshot_as_file(str(SHOTS / "01-first-run.png"))

        def fields():
            return d.find_elements(AppiumBy.CLASS_NAME, "XCUIElementTypeTextField")

        def field(prefix, index):
            fs = fields()
            for f in fs:
                if prefix in (f.get_attribute("label") or ""):
                    return f
            return fs[index] if len(fs) > index else fs[-1]

        def type_into(prefix, index, text):
            for _ in range(3):
                try:
                    field(prefix, index).click()
                    time.sleep(1)
                    d.switch_to.active_element.send_keys(text)
                    return True
                except StaleElementReferenceException:
                    time.sleep(1)
            return False

        note(f"host typed: {type_into('host', 1, host)}")
        note(f"token typed: {type_into('token', 2, token + chr(10))}")
        time.sleep(2)
        try:
            d.hide_keyboard()
        except Exception:  # noqa: BLE001
            pass
        d.get_screenshot_as_file(str(SHOTS / "02-filled.png"))
        clicked = False
        for _ in range(3):
            try:
                bs = d.find_elements(AppiumBy.IOS_PREDICATE, 'label == "Connect" OR name == "Connect"')
                if bs:
                    bs[0].click()
                    clicked = True
                break
            except StaleElementReferenceException:
                time.sleep(1)
        if not clicked:
            d.execute_script("mobile: tap", {"x": W // 2, "y": int(H * 0.63)})
        note(f"Connect clicked: {clicked}")
        time.sleep(15)
        d.get_screenshot_as_file(str(SHOTS / "03-now.png"))
        src = d.page_source
        note(f"paired: {'Nothing is waiting' in src or 'Needs you' in src or 'Working' in src}")

        def tap(x, y, wait=4):
            d.execute_script("mobile: tap", {"x": x, "y": y})
            time.sleep(wait)

        tab_y = int(H * 0.94)
        tap(W * 3 // 8, tab_y); d.get_screenshot_as_file(str(SHOTS / "04-chats.png"))
        tap(W * 5 // 8, tab_y, 6); d.get_screenshot_as_file(str(SHOTS / "05-box.png"))
        tap(W * 7 // 8, tab_y); d.get_screenshot_as_file(str(SHOTS / "06-me.png"))
        tap(W * 1 // 8, tab_y)
        tap(W // 2, int(H * 0.40), 8); d.get_screenshot_as_file(str(SHOTS / "07-chat.png"))
        note("done")
    finally:
        d.quit()
