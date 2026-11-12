# Run by LibreOffice's own Python: one picture per visible slide, reported
# line by line on stdout ("total N", "slide K", "error MESSAGE").
import os
import subprocess
import sys
import time

import uno
from com.sun.star.beans import PropertyValue
from com.sun.star.connection import NoConnectException

sys.stdout.reconfigure(encoding="utf-8")
env = os.environ
source, out = env["LC_SLIDES_IN"], env["LC_SLIDES_OUT"]
width, height = int(env["LC_SLIDES_W"]), int(env["LC_SLIDES_H"])
stop = os.path.join(out, "stop")


def prop(name, value):
    return PropertyValue(name, 0, value, 0)


# A private profile and pipe: a LibreOffice the person has open is left alone.
pipe = "livecraft%d" % os.getpid()
office = subprocess.Popen(
    [
        env["LC_SLIDES_OFFICE"],
        "-env:UserInstallation=" + uno.systemPathToFileUrl(env["LC_SLIDES_PROFILE"]),
        "--headless",
        "--invisible",
        "--norestore",
        "--nologo",
        "--nodefault",
        "--accept=pipe,name=%s;urp;" % pipe,
    ]
)
doc = None
desktop = None
try:
    local = uno.getComponentContext()
    resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
    ctx = None
    # The very first start builds the profile, which takes a while.
    deadline = time.time() + 120
    while ctx is None:
        try:
            ctx = resolver.resolve("uno:pipe,name=%s;urp;StarOffice.ComponentContext" % pipe)
        except NoConnectException:
            if office.poll() is not None or time.time() > deadline or os.path.exists(stop):
                raise RuntimeError("LibreOffice ne répond pas")
            time.sleep(0.1)
    smgr = ctx.ServiceManager
    desktop = smgr.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
    doc = desktop.loadComponentFromURL(
        uno.systemPathToFileUrl(source), "_blank", 0, (prop("Hidden", True), prop("ReadOnly", True))
    )
    if doc is None or not hasattr(doc, "getDrawPages"):
        raise RuntimeError("ce fichier n'est pas une présentation lisible")
    pages = doc.getDrawPages()
    shown = [pages.getByIndex(i) for i in range(pages.getCount())]
    shown = [p for p in shown if p.Visible]
    print("total %d" % len(shown), flush=True)
    exporter = smgr.createInstanceWithContext("com.sun.star.drawing.GraphicExportFilter", ctx)
    for n, page in enumerate(shown, 1):
        if os.path.exists(stop):
            break
        scale = min(width / page.Width, height / page.Height)
        size = (prop("PixelWidth", round(page.Width * scale)), prop("PixelHeight", round(page.Height * scale)))
        data = uno.Any("[]com.sun.star.beans.PropertyValue", size)
        target = uno.systemPathToFileUrl(os.path.join(out, "%d.png" % n))
        args = (prop("URL", target), prop("MediaType", "image/png"), PropertyValue("FilterData", 0, data, 0))
        exporter.setSourceDocument(page)
        uno.invoke(exporter, "filter", (args,))
        print("slide %d" % n, flush=True)
except Exception as e:
    print("error %s" % (getattr(e, "Message", "") or e), flush=True)
    sys.exit(1)
finally:
    try:
        if doc is not None:
            doc.close(True)
        if desktop is not None:
            desktop.terminate()
    except Exception:
        pass
    try:
        office.wait(20)
    except subprocess.TimeoutExpired:
        office.kill()
