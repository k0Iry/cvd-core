#!/usr/bin/env python3
"""Real-LWC Chromium replay; public recorded data, no production writes.

Requires playwright and Pillow in a separate test environment; uses an existing
Chromium executable. No npm or browser automation dependency is needed in the app.
Usage: python tests/browser-flow-smoke.py --fixture /path/to/history.json --output /tmp/cvd-flow
The fixture must be a captured /history JSON response with at least 1600 bars.
The local HTTP server appends a test-only bridge to app.js. It is never written
into production assets. Hidden-state tests simulate visibility events and are NOT
acceptance of Safari's compositor or the user's live deployment.
"""
import argparse
import copy
import hashlib
import http.server
import json
import threading
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from PIL import Image
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
BRIDGE = r"""
window.__cvdTest = {
 chart, page, priceSeries, cvdSeries, deltaPaneSeries,
 applyRealtimePayload, setBars, loadMoreBefore, reload,
 get hoveredTime() { return hoveredFlowTime; },
 get flowSeries() {
  return chart.panes()[2].getSeries().find(s => s.seriesType() === 'Custom');
 },
 writes: [], trace: [],
 instrument() {
  for (const pane of chart.panes()) for (const series of pane.getSeries()) {
   if (series.__instrumented) continue;
   series.__instrumented = true;
   for (const method of ['setData','update','applyOptions']) {
    const original = series[method].bind(series);
    series[method] = (...args) => { this.writes.push(method); return original(...args); };
   }
  }
 },
 range() { return chart.timeScale().getVisibleLogicalRange(); },
};
chart.subscribeCrosshairMove(p => window.__cvdTest.trace.push({time:p.time, source:!!p.sourceEvent, hovered:hoveredFlowTime}));
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--chromium', default='/usr/bin/chromium')
    args = parser.parse_args()
    payload = json.loads(args.fixture.read_text())
    bars = payload['bars']
    assert len(bars) >= 1600
    assert len({b['time'] for b in bars}) == len(bars)
    assert bars == sorted(bars, key=lambda b: b['time'])
    args.output.mkdir(parents=True, exist_ok=True)
    evidence = {'fixture_sha256': hashlib.sha256(args.fixture.read_bytes()).hexdigest(),
                'fixture_bars': len(bars), 'engine': 'Chromium with real vendored LWC/plugin',
                'scope': 'local recorded-history replay; synthetic visibility events; no production writes',
                'checks': [], 'page_errors': []}
    initial = bars[200:-2]
    requests = []

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=str(ROOT / 'static'), **kw)

        def log_message(self, format, *args):
            pass

        def do_GET(self):
            parsed = urlparse(self.path)
            if parsed.path == '/meta':
                return self.reply({'exchanges': [
                    {'exchange': 'binance', 'markets': ['spot','usdm','coinm'], 'symbols': ['BTC','ETH']},
                    {'exchange': 'coinbase', 'markets': ['spot','usdm'], 'symbols': ['BTC','ETH']}]})
            if parsed.path == '/history':
                q = parse_qs(parsed.query)
                requests.append(q)
                data = copy.deepcopy(payload)
                if 'before' in q:
                    data['bars'] = [b for b in bars if b['time'] < int(q['before'][0])]
                    data['has_more_before'] = False
                elif 'after' in q:
                    data['bars'] = [b for b in initial if b['time'] > int(q['after'][0])]
                    data['has_more_before'] = True
                else:
                    data['bars'] = initial
                    data['has_more_before'] = True
                data['has_more_after'] = False
                data['realtime_enabled'] = False
                data['oldest_time'] = data['bars'][0]['time'] if data['bars'] else None
                data['newest_time'] = data['bars'][-1]['time'] if data['bars'] else None
                return self.reply(data)
            if parsed.path == '/js/app.js':
                data = (ROOT / 'static/js/app.js').read_text() + BRIDGE
                raw = data.encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/javascript')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
                return
            if parsed.path == '/favicon.ico':
                self.send_response(204)
                self.end_headers()
                return
            return super().do_GET()

        def reply(self, data):
            raw = json.dumps(data).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.launch(executable_path=args.chromium, args=['--no-sandbox'])
            context = browser.new_context(viewport={'width': 1280, 'height': 960}, device_scale_factor=1)
            context.add_init_script("""
              window.__hidden = false;
              Object.defineProperty(document,'hidden',{get:()=>window.__hidden});
              Object.defineProperty(document,'visibilityState',{get:()=>window.__hidden?'hidden':'visible'});
            """)
            context.route('https://fonts.googleapis.com/**', lambda r: r.fulfill(body='', content_type='text/css'))
            page = context.new_page()
            page.on('pageerror', lambda e: evidence['page_errors'].append(str(e)))
            page.goto(f'http://127.0.0.1:{server.server_port}/', wait_until='networkidle')
            page.wait_for_function('window.__cvdTest && __cvdTest.page.bars.length > 1200')
            assert page.locator('#flow-mode').input_value() == 'delta'
            assert page.evaluate('__cvdTest.chart.panes().length') == 3
            assert page.evaluate("document.querySelectorAll('#toggle-rd,#toggle-kd,#toggle-fd,#toggle-hd,#signal-state').length") == 0
            assert page.evaluate("[...document.querySelectorAll('.pane-label')].map(e => e.firstChild.textContent.trim())")[:2] == ['Price', 'Cumulative Volume Delta (CVD)']
            assert page.evaluate("__cvdTest.chart.panes().every(p => p.getSeries().every(s => s.options().priceScaleId !== 'distance'))")
            core_version = page.evaluate("async () => (await import('lightweight-charts')).version()")
            assert core_version == '5.1.0', core_version
            evidence['core_version'] = core_version
            before = page.evaluate('JSON.stringify(__cvdTest.page.bars)')
            baseline = page.evaluate('[__cvdTest.priceSeries.data(),__cvdTest.cvdSeries.data()]')
            page.evaluate('__cvdTest.chart.timeScale().setVisibleLogicalRange({from:1200,to:1370})')
            page.wait_for_timeout(150)
            request_count = len(requests)
            mode_range = page.evaluate('__cvdTest.range()')
            page.select_option('#flow-mode', 'buy-sell')
            page.wait_for_timeout(200)
            assert page.evaluate('__cvdTest.range()') == mode_range, 'mode switch changed viewport'
            assert len(requests) == request_count, 'mode switch must not fetch history'
            assert page.evaluate('JSON.stringify(__cvdTest.page.bars)') == before
            assert page.evaluate('[__cvdTest.priceSeries.data(),__cvdTest.cvdSeries.data()]') == baseline
            assert page.evaluate('__cvdTest.flowSeries.options().scaleMode') == 'price'
            assert page.evaluate('__cvdTest.flowSeries.options().visible')
            assert not page.evaluate('__cvdTest.deltaPaneSeries.options().visible')
            point = page.evaluate('__cvdTest.flowSeries.data().at(-1)')
            b = initial[-1]
            assert point['values'] == [b['buy_volume_usdt'], 0, -b['sell_volume_usdt'], 0], point
            evidence['checks'].append('three panes (Price/LEVELS, CVD, Flow), no Strength controls; default Delta; switching to buy/sell preserves data, price/CVD and range; no request; numeric scale')
            page.locator('#chart').screenshot(path=str(args.output / 'buy-sell.png'))
            image = Image.open(args.output / 'buy-sell.png').convert('RGB')
            heights = page.evaluate('__cvdTest.chart.panes().map(p => p.getHeight())')
            flow = image.crop((0, int(sum(heights[:2])) + 12, image.width - 100,
                                 min(image.height, int(sum(heights)))))
            raw_pixels = flow.tobytes()
            colors = list(zip(raw_pixels[0::3], raw_pixels[1::3], raw_pixels[2::3]))
            green = sum(g > r * 1.25 and g > bl * 1.1 and g > 90 for r,g,bl in colors)
            red = sum(r > g * 1.4 and r > bl * 1.15 and r > 130 for r,g,bl in colors)
            assert green > 30 and red > 30, (green, red)
            evidence['rendered_color_pixels'] = {'green': green, 'red': red}
            page.check('#toggle-agg')
            page.wait_for_timeout(150)
            point = page.evaluate('__cvdTest.flowSeries.data().at(-1)')
            assert point['values'] == [b['agb'], 0, -b['ags'], 0], point
            assert page.evaluate('__cvdTest.cvdSeries.data().at(-1).value') == b['ac']
            evidence['checks'].append('Agg flow uses agb/ags; CVD still uses ac')
            page.evaluate('__cvdTest.instrument(); __cvdTest.writes.length=0')
            for b in bars[-2:]:
                page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"append",bar})', b)
            page.wait_for_timeout(50)
            assert page.evaluate('__cvdTest.page.bars.length') == len(initial) + 2
            assert page.evaluate('__cvdTest.flowSeries.data().at(-1).values') == [b['agb'],0,-b['ags'],0]
            assert 'setData' not in page.evaluate('__cvdTest.writes'), 'stream should update incrementally'
            count = page.evaluate('__cvdTest.page.bars.length')
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', b)
            assert page.evaluate('__cvdTest.page.bars.length') == count
            evidence['checks'].append('recorded bars replay as append/replace-last; incremental chart updates')
            # Constructed edge cases based on the last recorded bar, not market observations.
            invalid = {**b, 'agb': 0, 'ags': 0, 'ad': 1}
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', invalid)
            # LWC 5.1 custom-series data() omits whitespace rows; the prior
            # drawable bar may become at(-1). Assert the invalid time has no value.
            missing_point = page.evaluate('time => __cvdTest.flowSeries.data().find(p=>p.time===time) ?? null', b['time'])
            assert missing_point is None or not missing_point.get('values'), missing_point
            zero = {**b, 'agb': 0, 'ags': 0, 'ad': 0}
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', zero)
            assert page.evaluate('__cvdTest.flowSeries.data().at(-1).values') == [0,0,0,0]
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', b)
            assert page.evaluate('__cvdTest.flowSeries.data().at(-1).values') == [b['agb'],0,-b['ags'],0]
            evidence['checks'].append('constructed valid → unavailable whitespace → true zero → valid same-bar updates render without stale values')
            page.evaluate('__cvdTest.chart.timeScale().setVisibleLogicalRange({from:250,to:400})')
            page.wait_for_timeout(100)
            old_range = page.evaluate('__cvdTest.range()')
            page.evaluate('() => __cvdTest.loadMoreBefore()')
            page.wait_for_timeout(150)
            new_range = page.evaluate('__cvdTest.range()')
            assert abs(new_range['from'] - old_range['from'] - 200) < .01, (old_range,new_range)
            assert page.evaluate('__cvdTest.page.bars.length') == len(bars)
            evidence['checks'].append('older history prepend preserves logical viewport and complete series data')
            page.evaluate('__cvdTest.instrument(); __cvdTest.writes.length=0; window.__hidden=true; document.dispatchEvent(new Event("visibilitychange"))')
            page.select_option('#flow-mode', 'delta')
            page.select_option('#flow-mode', 'buy-sell')
            page.uncheck('#toggle-agg')
            for _ in range(5):
                page.evaluate('__cvdTest.setBars(__cvdTest.page.bars,__cvdTest.range())')
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', b)
            page.wait_for_timeout(100)
            assert page.evaluate('__cvdTest.writes.length') == 0, page.evaluate('__cvdTest.writes')
            page.evaluate('window.__hidden=false; document.dispatchEvent(new Event("visibilitychange"));window.dispatchEvent(new Event("focus"));window.dispatchEvent(new Event("pageshow"))')
            page.wait_for_timeout(200)
            assert page.evaluate('__cvdTest.flowSeries.data().at(-1).values') == [b['buy_volume_usdt'],0,-b['sell_volume_usdt'],0]
            assert page.evaluate('__cvdTest.flowSeries.options().visible')
            evidence['checks'].append('simulated hidden mode/Agg/history/live changes make zero chart writes; resume displays latest state')
            page.evaluate('__cvdTest.instrument();__cvdTest.writes.length=0;window.dispatchEvent(new Event("pagehide"))')
            page.evaluate('bar => __cvdTest.applyRealtimePayload({action:"replace_last",bar})', b)
            assert page.evaluate('__cvdTest.writes.length') == 0
            page.evaluate('window.dispatchEvent(new Event("pageshow"))')
            page.wait_for_timeout(100)
            evidence['checks'].append('pagehide blocks late chart writes before visibility changes')
            page.evaluate('__cvdTest.chart.timeScale().setVisibleLogicalRange({from:1450,to:1599})')
            page.wait_for_timeout(100)
            chart_box = page.locator('#chart').bounding_box()
            page.mouse.move(chart_box['x'] + chart_box['width'] * .7, chart_box['y'] + chart_box['height'] * .88)
            page.wait_for_timeout(100)
            summary = page.locator('#flow-summary').inner_text()
            assert summary.strip(), 'hover readout missing'
            evidence['hover_readout'] = summary
            with page.expect_download() as result:
                page.click('#screenshot-btn')
            result.value.save_as(str(args.output / 'export.png'))
            assert (args.output / 'export.png').read_bytes().startswith(b'\x89PNG')
            evidence['checks'].append('crosshair readout and chart PNG download work with real custom series')
            chart_box = page.locator('#chart').bounding_box()
            page.mouse.move(chart_box['x'] + chart_box['width'] * .6,
                            chart_box['y'] + chart_box['height'] * .88)
            page.wait_for_function('__cvdTest.hoveredTime !== null')
            page.evaluate('() => __cvdTest.reload()')
            page.wait_for_timeout(150)
            assert page.evaluate('__cvdTest.hoveredTime') is None, page.evaluate('__cvdTest.trace.slice(-12)')
            evidence['checks'].append('dataset reload resets active historical hover to latest-bar readout')
            context.set_default_timeout(10000)
            page.reload(wait_until='networkidle')
            page.wait_for_function('__cvdTest.page.bars.length > 1200')
            assert page.locator('#flow-mode').input_value() == 'buy-sell'
            page.emulate_media(color_scheme='dark')
            page.set_viewport_size({'width': 760, 'height': 900})
            page.wait_for_timeout(150)
            geometry = page.evaluate('''() => ({
                labelTop: document.querySelector('.flow-pane-label').getBoundingClientRect().top
                    - document.querySelector('#chart').getBoundingClientRect().top,
                expected: __cvdTest.chart.panes().slice(0,2).reduce((n,p)=>n+p.getHeight(),0) + 10,
            })''')
            assert abs(geometry['labelTop'] - geometry['expected']) < 14, geometry
            evidence['resize_label_geometry'] = geometry
            page.locator('#chart').screenshot(path=str(args.output / 'narrow-dark.png'))
            page.select_option('#flow-mode','delta')
            page.wait_for_timeout(100)
            assert page.evaluate('__cvdTest.deltaPaneSeries.options().visible')
            assert not page.evaluate('__cvdTest.flowSeries.options().visible')
            evidence['checks'].append('persisted mode survives reload; theme/resize and return to native Delta work')
            assert not evidence['page_errors'], evidence['page_errors']
            evidence['passed'] = True
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        (args.output / 'result.json').write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2))


if __name__ == '__main__':
    main()
