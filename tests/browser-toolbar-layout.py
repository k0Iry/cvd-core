#!/usr/bin/env python3
"""Real-browser regression checks for the CVD toolbar/chart viewport layout.

Serves only loopback HTTP, replays a recorded /history response, and appends a
bridge to the served app.js in memory. Production assets are never modified.
Synthetic hidden/pageshow events do not reproduce or certify Safari compositing.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import http.server
import json
import subprocess
import threading
import traceback
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
BRIDGE = r"""
window.__cvdLayoutTest = {
  chart, page,
  panes() { return chart.panes().map(p => { const s=p.getSeries(); return {height:p.getHeight(),seriesCount:s.length,types:s.map(x=>x.seriesType()),maxPoints:Math.max(0,...s.map(x=>x.data().length))}; }); }
};
"""

VIEWPORTS = [
    ("wide", 1280, 960),
    ("wrapped", 760, 680),
    ("mobile", 390, 844),
]


def git_asset(revision: str, relative: str) -> bytes:
    return subprocess.check_output(["git", "show", f"{revision}:{relative}"], cwd=ROOT)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--engine", choices=("chromium", "webkit"), default="chromium")
    ap.add_argument("--chromium", default="/usr/bin/chromium")
    ap.add_argument("--baseline", metavar="REV", help="serve index/CSS/app.js from this git revision")
    args = ap.parse_args()

    fixture_bytes = args.fixture.read_bytes()
    payload = json.loads(fixture_bytes)
    bars = payload["bars"]
    if len(bars) < 1600:
        ap.error(f"fixture needs >=1600 bars, got {len(bars)}")
    if bars != sorted(bars, key=lambda b: b["time"]):
        ap.error("fixture bars are not time-sorted")
    if len({b["time"] for b in bars}) != len(bars):
        ap.error("fixture contains duplicate times")
    args.output.mkdir(parents=True, exist_ok=True)

    overrides: dict[str, bytes] = {}
    if args.baseline:
        for rel in ("static/index.html", "static/css/app.css", "static/js/app.js"):
            overrides["/" + rel.removeprefix("static/")] = git_asset(args.baseline, rel)

    initial = bars[-1600:]

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=str(ROOT / "static"), **kw)

        def log_message(self, *_):
            pass

        def send_bytes(self, raw: bytes, content_type: str):
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            parsed = urlparse(self.path)
            path = parsed.path
            if path == "/meta":
                return self.send_json({"exchanges": [{"exchange": "binance", "markets": ["spot", "usdm", "coinm"], "symbols": ["BTC", "ETH"]}]})
            if path == "/history":
                q = parse_qs(parsed.query)
                data = copy.deepcopy(payload)
                selected = initial
                if "before" in q:
                    selected = [b for b in bars if b["time"] < int(q["before"][0])][-1600:]
                elif "after" in q:
                    selected = [b for b in initial if b["time"] > int(q["after"][0])]
                data.update(bars=selected, has_more_before=False, has_more_after=False,
                            realtime_enabled=False,
                            oldest_time=selected[0]["time"] if selected else None,
                            newest_time=selected[-1]["time"] if selected else None)
                return self.send_json(data)
            if path == "/favicon.ico":
                self.send_response(204); self.end_headers(); return
            if path == "/js/app.js":
                raw = overrides.get(path, (ROOT / "static/js/app.js").read_bytes()) + BRIDGE.encode()
                return self.send_bytes(raw, "text/javascript; charset=utf-8")
            if path in overrides:
                kind = "text/html" if path == "/index.html" else "text/css"
                return self.send_bytes(overrides[path], kind + "; charset=utf-8")
            if path == "/" and "/index.html" in overrides:
                return self.send_bytes(overrides["/index.html"], "text/html; charset=utf-8")
            return super().do_GET()

        def send_json(self, value):
            self.send_bytes(json.dumps(value).encode(), "application/json")

    result = {
        "passed": False,
        "engine": args.engine,
        "baseline_revision": args.baseline,
        "fixture": str(args.fixture),
        "fixture_sha256": hashlib.sha256(fixture_bytes).hexdigest(),
        "fixture_bars": len(bars),
        "scope": "loopback real browser + vendored LWC; visibility/pageshow is simulated, not Safari compositor reproduction",
        "scenarios": [],
        "errors": [],
        "page_errors": [],
    }
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    def capture_geometry(page, name: str, short=False):
        data = page.evaluate("""() => {{
          const short = {short};
          const r = e => {{ const b=e.getBoundingClientRect(); return {{x:b.x,y:b.y,width:b.width,height:b.height,top:b.top,right:b.right,bottom:b.bottom,left:b.left}} }};
          const bar=document.querySelector('.bar'), chart=document.querySelector('#chart'), wrap=document.querySelector('#wrap');
          const controls=[...document.querySelectorAll('.bar input,.bar select,.bar button,#flow-mode')].map(e => {{
            const b=r(e), cx=Math.max(0,Math.min(innerWidth-1,b.left+b.width/2)), cy=Math.max(0,Math.min(innerHeight-1,b.top+b.height/2));
            const hit=document.elementFromPoint(cx,cy); return {{id:e.id,box:b,hit:hit===e||e.contains(hit),visible:b.bottom>0&&b.top<innerHeight&&b.right>0&&b.left<innerWidth}};
          }});
          const canvases=[...chart.querySelectorAll('canvas')].map(r), cb=r(chart), bb=r(bar), wb=r(wrap);
          return {{name:{name},short,viewport:{{width:innerWidth,height:innerHeight}},scroll:{{x:scrollX,y:scrollY,sw:document.documentElement.scrollWidth,sh:document.documentElement.scrollHeight}},bar:bb,barScroll:{{top:bar.scrollTop,height:bar.clientHeight,full:bar.scrollHeight}},wrap:wb,chart:cb,controls,canvases,panes:__cvdLayoutTest.panes()}};
        }}""".format(short="true" if short else "false", name=json.dumps(name)))
        return data

    def assert_ordinary(g):
        w, h = g["viewport"]["width"], g["viewport"]["height"]
        failures = []
        if g["scroll"]["sw"] > w + 1 or g["scroll"]["sh"] > h + 1 or abs(g["scroll"]["x"]) > 1 or abs(g["scroll"]["y"]) > 1:
            failures.append(f"document scroll/overflow {g['scroll']} vs viewport {w}x{h}")
        if g["bar"]["top"] < -1:
            failures.append(f"toolbar top is hidden: {g['bar']['top']:.1f}px")
        bad = [c["id"] for c in g["controls"] if not c["visible"] or not c["hit"]]
        if bad:
            failures.append(f"controls outside viewport or not hit-testable: {bad}")
        if g["chart"]["top"] < g["bar"]["bottom"] - 1:
            failures.append(f"chart overlaps toolbar: chart top {g['chart']['top']:.1f}, toolbar bottom {g['bar']['bottom']:.1f}")
        if g["chart"]["bottom"] > h + 1 or g["chart"]["left"] < -1 or g["chart"]["right"] > w + 1:
            failures.append(f"chart outside viewport: {g['chart']}")
        for i, c in enumerate(g["canvases"]):
            if c["left"] < g["chart"]["left"] - 1 or c["right"] > g["chart"]["right"] + 1 or c["top"] < g["chart"]["top"] - 1 or c["bottom"] > g["chart"]["bottom"] + 1:
                failures.append(f"canvas {i} escapes chart: {c}")
        panes = g["panes"]
        if len(panes) != 3 or any(not p["seriesCount"] or p["maxPoints"] != len(initial) for p in panes):
            failures.append(f"expected three populated panes (Price, CVD, Flow), got {panes}")
        return failures

    try:
        with sync_playwright() as pw:
            launcher = getattr(pw, args.engine)
            launch_kw = {"headless": True}
            if args.engine == "chromium":
                launch_kw.update(executable_path=args.chromium, args=["--no-sandbox"])
            browser = launcher.launch(**launch_kw)
            context = browser.new_context(viewport={"width": 1280, "height": 960}, device_scale_factor=1)
            context.add_init_script("""
              window.__hidden=false;
              Object.defineProperty(document,'hidden',{get:()=>window.__hidden});
              Object.defineProperty(document,'visibilityState',{get:()=>window.__hidden?'hidden':'visible'});
            """)
            context.route("https://fonts.googleapis.com/**", lambda r: r.fulfill(body="", content_type="text/css"))
            page = context.new_page()
            page.on("pageerror", lambda e: result["page_errors"].append(str(e)))
            page.goto(f"http://127.0.0.1:{server.server_port}/", wait_until="networkidle")
            page.wait_for_function("window.__cvdLayoutTest && __cvdLayoutTest.page.bars.length >= 1600")

            for name, width, height in VIEWPORTS:
                page.set_viewport_size({"width": width, "height": height})
                page.wait_for_timeout(180)
                page.evaluate("scrollTo(0,0)")
                g = capture_geometry(page, name)
                failures = assert_ordinary(g)
                # Wheel root at toolbar gutter, then at chart edge: neither may scroll the document.
                for x, y in ((width - 3, min(height - 2, g["bar"]["top"] + 3)),
                             (width - 3, min(height - 2, g["chart"]["top"] + 10))):
                    page.mouse.move(x, y); page.mouse.wheel(0, 600); page.wait_for_timeout(60)
                wheel_scroll = page.evaluate("({x:scrollX,y:scrollY,sw:document.documentElement.scrollWidth,sh:document.documentElement.scrollHeight})")
                if abs(wheel_scroll["x"]) > 1 or abs(wheel_scroll["y"]) > 1:
                    failures.append(f"wheel scrolled document: {wheel_scroll}")
                page.evaluate("scrollTo(0,0)")
                page.screenshot(path=str(args.output / f"{name}.png"), full_page=True)
                g["wheel_scroll"] = wheel_scroll; g["failures"] = failures
                result["scenarios"].append(g); result["errors"].extend(f"{name}: {x}" for x in failures)

            # Resize churn, scroll attempt, then simulated lifecycle restore.
            for width, height in ((760, 680), (1280, 960), (390, 844), (760, 680)):
                page.set_viewport_size({"width": width, "height": height}); page.wait_for_timeout(80)
            page.evaluate("window.__hidden=true;document.dispatchEvent(new Event('visibilitychange'))")
            page.set_viewport_size({"width": 390, "height": 844})
            page.evaluate("window.__hidden=false;document.dispatchEvent(new Event('visibilitychange'));window.dispatchEvent(new Event('pageshow'))")
            page.wait_for_timeout(500)
            restored = capture_geometry(page, "restored")
            restored["failures"] = assert_ordinary(restored)
            result["scenarios"].append(restored); result["errors"].extend(f"restored: {x}" for x in restored["failures"])
            page.screenshot(path=str(args.output / "restored.png"), full_page=True)

            # Very short view: every control must remain reachable by toolbar-internal scrolling.
            page.set_viewport_size({"width": 390, "height": 360}); page.wait_for_timeout(150); page.evaluate("scrollTo(0,0)")
            short = capture_geometry(page, "very-short", True)
            short_fail = []
            flow_height = next(c["box"]["height"] for c in short["controls"] if c["id"] == "flow-mode")
            if short["chart"]["height"] < 280 or any(p["height"] < flow_height + 16 for p in short["panes"]):
                short_fail.append(f"chart panes squeezed below readable height: {[p['height'] for p in short['panes']]}")
            if short["bar"]["top"] < -1:
                short_fail.append("toolbar top hidden in short viewport")
            if short["scroll"]["sw"] > 391 or abs(short["scroll"]["x"]) > 1 or abs(short["scroll"]["y"]) > 1:
                short_fail.append(f"document, rather than toolbar, scrolls: {short['scroll']}")
            unreachable = []
            for control in page.locator(".bar input,.bar select,.bar button").all():
                control.scroll_into_view_if_needed(); page.wait_for_timeout(20)
                ok = control.evaluate("e=>{const b=e.getBoundingClientRect(),x=b.left+b.width/2,y=b.top+b.height/2,h=document.elementFromPoint(x,y);return b.top>=0&&b.bottom<=innerHeight&&b.left>=0&&b.right<=innerWidth&&(h===e||e.contains(h))}")
                if not ok: unreachable.append(control.get_attribute("id"))
            if unreachable:
                short_fail.append(f"controls unreachable via internal toolbar scroll: {unreachable}")
            short["after_internal_scroll"] = page.evaluate("({x:scrollX,y:scrollY,toolbarScroll:document.querySelector('.bar').scrollTop})")
            if abs(short["after_internal_scroll"]["x"]) > 1 or abs(short["after_internal_scroll"]["y"]) > 1:
                short_fail.append("control focus/scroll moved the root document")
            short["failures"] = short_fail
            result["scenarios"].append(short); result["errors"].extend(f"very-short: {x}" for x in short_fail)
            page.screenshot(path=str(args.output / "very-short.png"), full_page=True)

            # Below the combined minimum, only #wrap may scroll to reach the
            # flow pane. Focusing toolbar controls must not move the document.
            page.set_viewport_size({"width": 390, "height": 240})
            page.wait_for_timeout(180)
            page.locator('#flow-mode').scroll_into_view_if_needed()
            page.wait_for_timeout(80)
            tiny = capture_geometry(page, "chart-internal-scroll", True)
            tiny_fail = []
            flow = next(c for c in tiny["controls"] if c["id"] == 'flow-mode')
            if tiny["chart"]["height"] < 280 or not flow["hit"]:
                tiny_fail.append('minimum chart height / flow-pane access lost')
            if tiny["bar"]["top"] < -1 or abs(tiny["scroll"]["y"]) > 1 or tiny["scroll"]["sh"] > 241:
                tiny_fail.append('reaching flow pane moved root document or toolbar')
            tiny["wrapScroll"] = page.locator('#wrap').evaluate('e => e.scrollTop')
            if tiny["wrapScroll"] <= 0:
                tiny_fail.append('expected internal chart-area scrolling')
            tiny["failures"] = tiny_fail
            result["scenarios"].append(tiny)
            result["errors"].extend(f"chart-internal-scroll: {x}" for x in tiny_fail)
            page.screenshot(path=str(args.output / "chart-internal-scroll.png"), full_page=True)
            browser.close()
        if result["page_errors"]:
            result["errors"].append("page errors: " + repr(result["page_errors"]))
        result["passed"] = not result["errors"]
    except Exception as exc:
        result["errors"].append(f"harness exception: {exc}")
        result["traceback"] = traceback.format_exc()
    finally:
        server.shutdown(); server.server_close()
        (args.output / "result.json").write_text(json.dumps(result, indent=2))

    print(json.dumps({
        "passed": result["passed"], "engine": result["engine"],
        "baseline_revision": result["baseline_revision"],
        "scenarios": [{"name": s["name"], "viewport": s["viewport"],
                       "scroll": s["scroll"], "failures": s["failures"]}
                      for s in result["scenarios"]],
        "errors": result["errors"], "page_errors": result["page_errors"],
        "evidence": str(args.output / "result.json"),
    }, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
