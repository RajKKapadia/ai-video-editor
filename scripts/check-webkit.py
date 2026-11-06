import gi
import json
import sys

# Optional Linux test: uses an ephemeral profile in Tauri's WebKitGTK engine.
passed = False
gi.require_version('Gtk', '3.0')
gi.require_version('WebKit2', '4.1')
from gi.repository import Gtk, WebKit2, GLib
context = WebKit2.WebContext.new_ephemeral()
view = WebKit2.WebView.new_with_context(context)
view.get_settings().set_media_playback_requires_user_gesture(False)
window = Gtk.Window(title='Cutline playback verification')
window.set_default_size(1280, 900)
window.add(view)
window.show_all()

def finish(webview, result, _):
    global passed
    try:
        result = json.loads(webview.evaluate_javascript_finish(result).to_string())
        print(json.dumps(result), flush=True)
        passed = result.get('currentTime', 0) >= 4 and result.get('totalFrames', 0) > 0 and not result.get('error')
    except Exception as error:
        print(str(error), flush=True)
    Gtk.main_quit()

def measure():
    view.evaluate_javascript("JSON.stringify((()=>{const v=document.querySelector('video'); if(!v)return {error:document.body.innerText.slice(0,500)};const q=v.getVideoPlaybackQuality();return {currentTime:v.currentTime,paused:v.paused,totalFrames:q.totalVideoFrames,droppedFrames:q.droppedVideoFrames,error:v.error?.message};})())", -1, None, None, None, finish, None)
    return False

def play():
    view.evaluate_javascript("document.querySelector('[aria-label=\"Play preview\"]')?.click();", -1, None, None, None, None, None)
    GLib.timeout_add_seconds(6, measure)
    return False

def loaded(webview, event):
    if event == WebKit2.LoadEvent.FINISHED:
        GLib.timeout_add_seconds(2, play)
view.connect('load-changed', loaded)
view.load_uri('http://127.0.0.1:1420/#project=demo-project')
GLib.timeout_add_seconds(20, lambda: (Gtk.main_quit(), False)[1])
Gtk.main()
window.destroy()

sys.exit(0 if passed else 1)
