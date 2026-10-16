# Nautilus "Send via QuickShare" menu item. Installed by the .deb to
# /usr/share/nautilus-python/extensions/ (needs the nautilus-python package).
import subprocess

from gi.repository import GObject, Nautilus


class QuickShareMenu(GObject.GObject, Nautilus.MenuProvider):
    def get_file_items(self, files):
        paths = [f.get_location().get_path() for f in files]
        paths = [p for p in paths if p]
        if not paths:
            return []
        item = Nautilus.MenuItem(name="QuickShare::send", label="Send via QuickShare")
        item.connect("activate", lambda *_: subprocess.Popen(["quickshare-linux", *paths]))
        return [item]
