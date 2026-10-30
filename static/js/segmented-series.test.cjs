const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, 'chart.js'), 'utf8').replace(/^export /gm, ''), context);
const call = (name, ...args) => context[name](...args);
const plain = value => JSON.parse(JSON.stringify(value));

function recorder() {
    const ops = [];
    const ctx = {
        set lineCap(v) { ops.push(['lineCap', v]); }, set lineJoin(v) { ops.push(['lineJoin', v]); },
        set lineWidth(v) { ops.push(['lineWidth', v]); }, set strokeStyle(v) { ops.push(['strokeStyle', v]); },
        setLineDash: v => ops.push(['setLineDash', v]), beginPath: () => ops.push(['beginPath']),
        moveTo: (x, y) => ops.push(['moveTo', x, y]), lineTo: (x, y) => ops.push(['lineTo', x, y]), stroke: () => ops.push(['stroke']),
    };
    return { ops, scope: { context: ctx, horizontalPixelRatio: 2, verticalPixelRatio: 3 } };
}
// LWC 5.1 custom pane data: bars carry x only inside visibleRange, time is the
// time-point index; whitespace items are not drawable.
function frame(items, from, to, spacing = 10, firstIndex = 0) {
    const bars = items.map((item, i) => ({ time: firstIndex + item.index, x: i >= from && i < to ? 100 + (firstIndex + item.index) * spacing : NaN, originalData: item }));
    return { bars, visibleRange: { from, to }, barSpacing: spacing };
}
const y = value => 1000 - value;
const paths = ops => ops.filter(op => op[0] === 'moveTo' || op[0] === 'lineTo');

test('segments: singletons suppressed, zero kept, null/NaN/Infinity gaps never bridged', () => {
    const bars = [1, 2, 3, 4, 5, 6, 7, 8, 9].map(time => ({ time }));
    const values = [1, 0, null, 5, NaN, 6, 7, Infinity, 9];
    assert.deepEqual(plain(call('segmentData', bars, values)), [[{ time: 1, value: 1 }, { time: 2, value: 0 }], [{ time: 6, value: 6 }, { time: 7, value: 7 }]]);
});

test('slot split: history carries explicit run starts, tail is the final segment, canonicalization round-trips', () => {
    const segs = [[{ time: 1, value: 1 }, { time: 2, value: 2 }], [{ time: 4, value: 0 }, { time: 5, value: 3 }, { time: 6, value: 4 }], [{ time: 8, value: 5 }, { time: 9, value: 6 }]];
    const { history, tail } = call('segmentSlotData', segs);
    assert.deepEqual(plain(history), [
        { time: 1, value: 1, start: true }, { time: 2, value: 2, start: false },
        { time: 4, value: 0, start: true }, { time: 5, value: 3, start: false }, { time: 6, value: 4, start: false }]);
    assert.deepEqual(plain(tail), segs[2]);
    assert.deepEqual(plain([...call('segmentHistorySegments', history), tail]), segs, 'per-run parity with native segments');
    assert.deepEqual(plain(call('segmentSlotData', [segs[0]])), { history: [], tail: segs[0] });
    assert.deepEqual(plain(call('segmentSlotData', [])), { history: [], tail: null });
    // Randomized parity: canonical history + tail === original segments.
    let seed = 7; const rnd = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
    for (let round = 0; round < 50; round++) {
        const bars = Array.from({ length: 200 }, (_, i) => ({ time: (i + 1) * 60 }));
        const values = bars.map(() => rnd() < 0.15 ? null : Math.round(rnd() * 100) - 50);
        const native = call('segmentData', bars, values);
        const split = call('segmentSlotData', native);
        assert.deepEqual(plain(native.length ? [...call('segmentHistorySegments', split.history), split.tail] : []), plain(native));
        assert.ok(call('segmentHistorySegments', split.history).every(segment => segment.length > 1));
    }
});

test('sameSegmentData compares time/value/start exactly', () => {
    const a = [{ time: 1, value: 0, start: true }, { time: 2, value: 1, start: false }];
    assert.equal(call('sameSegmentData', a, a.map(p => ({ ...p }))), true);
    assert.equal(call('sameSegmentData', a, [{ ...a[0], start: false }, a[1]]), false);
    assert.equal(call('sameSegmentData', a, [{ ...a[0], value: 1e-9 }, a[1]]), false);
    assert.equal(call('sameSegmentData', [], []), true);
    assert.equal(call('sameSegmentData', a, a.slice(1)), false);
});

test('draw: one moveTo per run, no bridge between runs, native line styling in bitmap space', () => {
    const items = [
        { index: 0, time: 1, value: 10, start: true }, { index: 1, time: 2, value: 0, start: false },
        { index: 3, time: 4, value: 20, start: true }, { index: 4, time: 5, value: 30, start: false }, { index: 5, time: 6, value: 40, start: false }];
    const { ops, scope } = recorder();
    call('drawSegmentHistory', scope, frame(items, 0, 5), { color: '#eb5757', lineWidth: 4 }, y);
    assert.deepEqual(plain(ops.slice(0, 6)), [['lineCap', 'butt'], ['lineJoin', 'round'], ['lineWidth', 12], ['strokeStyle', '#eb5757'], ['setLineDash', []], ['beginPath']]);
    assert.deepEqual(paths(ops), [
        ['moveTo', 200, 2970], ['lineTo', 220, 3000],
        ['moveTo', 260, 2940], ['lineTo', 280, 2910], ['lineTo', 300, 2880]]);
    assert.deepEqual(ops.at(-1), ['stroke']);
    assert.equal(ops.filter(op => op[0] === 'moveTo').length, 2);
});

test('draw: clips at viewport edges using one neighbour each side positioned from the time index', () => {
    const items = Array.from({ length: 8 }, (_, index) => ({ index, time: index, value: index, start: index === 0 || index === 5 }));
    const { ops, scope } = recorder();
    // Visible 2..4 (bars 2,3) => draw 1..4; neighbours 1 and 4 get extrapolated x.
    call('drawSegmentHistory', scope, frame(items, 2, 4), { color: '#27ae60', lineWidth: 2 }, y);
    assert.deepEqual(paths(ops), [['moveTo', 220, 2997], ['lineTo', 240, 2994], ['lineTo', 260, 2991], ['lineTo', 280, 2988]]);
    // A run starting right after the viewport is entered with moveTo, not bridged.
    const r2 = recorder();
    call('drawSegmentHistory', r2.scope, frame(items, 3, 5), { color: '#27ae60', lineWidth: 2 }, y);
    assert.deepEqual(paths(r2.ops), [['moveTo', 240, 2994], ['lineTo', 260, 2991], ['lineTo', 280, 2988], ['moveTo', 300, 2985]]);
    // Empty visible range draws nothing.
    const r3 = recorder();
    call('drawSegmentHistory', r3.scope, frame(items, 4, 4), { color: '#27ae60', lineWidth: 2 }, y);
    assert.deepEqual(r3.ops, []);
});

test('draw: null coordinate breaks the path instead of bridging', () => {
    const items = [0, 1, 2].map(index => ({ index, time: index, value: index + 1, start: index === 0 }));
    const { ops, scope } = recorder();
    call('drawSegmentHistory', scope, frame(items, 0, 3), { color: 'c', lineWidth: 1 }, v => v === 2 ? null : y(v));
    assert.deepEqual(paths(ops), [['moveTo', 200, 2997], ['moveTo', 240, 2991]]);
});

// Command stream split into [beginPath ... stroke] groups of path ops.
const strokes = ops => {
    const out = []; let cur = null;
    for (const op of ops) {
        if (op[0] === 'beginPath') cur = [];
        else if (op[0] === 'stroke') { out.push(cur); cur = null; }
        else if (op[0] === 'moveTo' || op[0] === 'lineTo') cur.push(op);
    }
    return out;
};

test('draw: all runs share one batched stroke; a lone start is moveTo-only; gaps, zero and empty range preserved', () => {
    const items = [
        { index: 0, time: 0, value: 1, start: true }, { index: 1, time: 1, value: 2, start: false },
        { index: 3, time: 3, value: 0, start: true },
        { index: 5, time: 5, value: 4, start: true }, { index: 6, time: 6, value: 5, start: false }];
    const levels = recorder();
    call('drawSegmentHistory', levels.scope, frame(items, 0, 5), { color: 'c', lineWidth: 1 }, y);
    assert.deepEqual(plain(levels.ops.slice(0, 5)), [['lineCap', 'butt'], ['lineJoin', 'round'], ['lineWidth', 3], ['strokeStyle', 'c'], ['setLineDash', []]]);
    assert.deepEqual(strokes(levels.ops), [[['moveTo', 200, 2997], ['lineTo', 220, 2994], ['moveTo', 260, 3000], ['moveTo', 300, 2988], ['lineTo', 320, 2985]]]);
    const e = recorder();
    call('drawSegmentHistory', e.scope, frame(items, 4, 4), { color: 'g', lineWidth: 2 }, y);
    assert.deepEqual(e.ops, []);
});

test('custom pane view follows the LWC 5.1 ICustomSeriesPaneView contract', () => {
    const view = call('createSegmentHistoryView', { lastValueVisible: true, priceLineVisible: true, visible: true });
    for (const key of ['renderer', 'update', 'priceValueBuilder', 'isWhitespace', 'defaultOptions']) assert.equal(typeof view[key], 'function', key);
    assert.deepEqual(plain(view.priceValueBuilder({ time: 1, value: 0, start: true })), [0], 'value-based autoscale includes zero');
    assert.equal(view.isWhitespace({ time: 1 }), true);
    assert.equal(view.isWhitespace({ time: 1, value: 0 }), false);
    assert.equal(view.defaultOptions().visible, true);
    const { ops, scope } = recorder();
    const target = { useBitmapCoordinateSpace: fn => fn(scope) };
    view.renderer().draw(target, y);
    assert.deepEqual(ops, [], 'no draw before update');
    view.update(frame([{ index: 0, time: 0, value: 1, start: true }, { index: 1, time: 1, value: 2, start: false }], 0, 2), { color: 'c', lineWidth: 1 });
    view.renderer().draw(target, y);
    assert.equal(paths(ops).length, 2);
});
