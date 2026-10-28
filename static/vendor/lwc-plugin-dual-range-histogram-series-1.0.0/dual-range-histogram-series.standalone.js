import { customSeriesDefaultOptions as e } from "lightweight-charts";
//#region ../lwc-toolkit/dist/custom-series/visible-bars.js
function t(e, t) {
	return e === null ? {
		from: 0,
		to: 0
	} : {
		from: Math.max(0, e.from),
		to: Math.min(t, e.to)
	};
}
function n(e, n) {
	let { from: r, to: i } = t(e.visibleRange, e.bars.length);
	if (r >= i) return [];
	let a = Array(i - r);
	for (let t = r; t < i; t++) a[t - r] = n(e.bars[t], t);
	return a;
}
function r(e) {
	return Math.max(1, e.conflationFactor ?? 1);
}
//#endregion
//#region ../lwc-toolkit/dist/custom-series/options-aware-series.js
function i(e, t, n, r, i, a = 0) {
	let o = [], s = {
		points: [],
		revision: 0
	}, c = () => s, l = (e) => {
		let t = 0, n = o.length;
		for (; t < n;) {
			let r = t + n >> 1;
			o[r] < e ? t = r + 1 : n = r;
		}
		return t;
	}, u, d = t(() => u?.options() ?? n, c);
	u = e.addCustomSeries(d, r, a);
	let f = u, p = (t) => e.horzBehaviour().key(t.time), m = [], h = !1, g = f.setData.bind(f), _ = f.update.bind(f), v = f.applyOptions.bind(f), y = () => {
		!h || m.length !== 0 || (h = !1, s.points.length > 0 && g(s.points.slice()));
	};
	f.subscribeDataChanged(() => {
		let e = m[m.length - 1], t = e?.commit;
		t && (e.commit = null, t(), s.revision++);
	});
	let b = (e, t) => {
		m.push({ commit: e });
		try {
			return t();
		} finally {
			m.pop(), y();
		}
	};
	f.setData = (e) => {
		let t = e.map((e) => ({
			time: p(e),
			point: { ...e }
		})).sort((e, t) => e.time - t.time);
		b(() => {
			o = t.map((e) => e.time), s.points = t.map((e) => e.point);
		}, () => g(e));
	}, f.update = (e, t) => {
		let n = p(e), r = { ...e };
		b(() => {
			let e = l(n);
			o[e] === n ? s.points[e] = r : (o.splice(e, 0, n), s.points.splice(e, 0, r));
		}, () => _(e, t));
	};
	let x = f, S = x.pop?.bind(f);
	return S !== void 0 && (x.pop = (e) => {
		let t = e <= 0 ? [] : f.data().slice(-e).map(p);
		return b(() => {
			for (let e of t) {
				let t = l(e);
				o[t] === e && (o.splice(t, 1), s.points.splice(t, 1));
			}
		}, () => S(e));
	}), f.applyOptions = (e) => {
		let t = i.map((e) => f.options()[e]);
		v(e), i.some((e, n) => f.options()[e] !== t[n]) && (h = !0, y());
	}, f;
}
//#endregion
//#region src/options.ts
var a = [
	"upOuter",
	"upInner",
	"downOuter",
	"downInner"
], o = {
	...e,
	colors: {
		upOuter: "#ACE5DC",
		upInner: "#42BDA8",
		downOuter: "#FCCACD",
		downInner: "#F77C80"
	},
	borderRadius: {
		upOuter: 2,
		upInner: 0,
		downOuter: 2,
		downInner: 0
	},
	borderColor: null,
	borderWidth: 1,
	maxHeight: 130,
	scaleMode: "pixels",
	normalize: "visible",
	gap: 0,
	widthPercent: 100,
	baseValue: 0,
	highlightHovered: !1
};
//#endregion
//#region ../lwc-toolkit/dist/canvas/round-rect.js
function s(e, t) {
	return e.map((e) => Math.max(0, e + t));
}
function c(e, t, n) {
	return Math.floor(Math.min(e, t / 2, Math.abs(n)));
}
function l(e, t, n, r, i, a) {
	e.beginPath(), e.roundRect(t, n, r, i, a);
}
function u(e, t, n, r, i, a, o = 0, c = [
	0,
	0,
	0,
	0
], u) {
	if (r === 0 || i === 0) return;
	if (o = Math.min(Math.max(0, o), Math.abs(r), Math.abs(i)), e.save(), o === 0 || u === void 0 || u === a) {
		l(e, t, n, r, i, c), e.fillStyle = a, e.fill(), e.restore();
		return;
	}
	let d = o / 2, f = s(c, -d);
	l(e, t + d, n + d, r - o, i - o, f), e.fillStyle = a, e.fill(), e.lineWidth = o, e.strokeStyle = u, e.stroke(), e.restore();
}
//#endregion
//#region ../lwc-toolkit/dist/dimensions/columns.js
var d = 4, f = 1;
function p(e, t) {
	return Math.ceil(e * t) <= f ? 0 : Math.max(1, Math.floor(t));
}
function m(e, t, n) {
	return Math.round(e * t) - (n ?? p(e, t));
}
function h(e, t) {
	let n = p(e, t), r = m(e, t, n), i = r % 2 == 0;
	return {
		spacing: n,
		shiftLeft: i,
		columnHalfWidthBitmap: (r - +!i) / 2,
		horizontalPixelRatio: t
	};
}
function g(e, t, n) {
	let r = e * t.horizontalPixelRatio, i = Math.round(r), a = {
		left: i - t.columnHalfWidthBitmap,
		right: i + t.columnHalfWidthBitmap - +!!t.shiftLeft,
		shiftLeft: i > r
	}, o = t.spacing + 1;
	return n && a.left - n.right !== o && (n.shiftLeft ? n.right = a.left - o : a.left = n.right + o), a;
}
function _(e, t, n) {
	return e.time === void 0 || t.time === void 0 || e.time === t.time + n;
}
function v(e, t, n, r, i, a = 1) {
	let o = h(t, n), s = Math.min(i, e.length), c;
	for (let t = r; t < s; t++) {
		let n = c !== void 0 && _(e[t], e[t - 1], a) ? c : void 0;
		e[t].column = g(e[t].x, o, n), c = e[t].column;
	}
	let l = Math.ceil(t * n);
	for (let t = r; t < s; t++) {
		let n = e[t].column;
		n && (n.right < n.left && (n.right = n.left), l = Math.min(l, n.right - n.left + 1));
	}
	if (o.spacing > 0 && l < d) for (let t = r; t < s; t++) {
		let n = e[t].column;
		n && (n.right - n.left + 1 <= l || (n.shiftLeft ? --n.right : n.left += 1));
	}
}
//#endregion
//#region ../lwc-toolkit/dist/dimensions/positions.js
function y(e, t, n) {
	let r = Math.round(n * e), i = Math.round(n * t);
	return {
		position: Math.min(r, i),
		length: Math.abs(i - r) + 1
	};
}
//#endregion
//#region ../lwc-toolkit/dist/custom-series/renderer-base.js
var b = class {
	constructor() {
		Object.defineProperty(this, "data", {
			enumerable: !0,
			configurable: !0,
			writable: !0,
			value: null
		}), Object.defineProperty(this, "options", {
			enumerable: !0,
			configurable: !0,
			writable: !0,
			value: null
		});
	}
	update(e, t) {
		this.data = e, this.options = t;
	}
	draw(e, t, n, r) {
		let i = this.data, a = this.options;
		if (i === null || a === null || i.bars.length === 0) return;
		let o = i.visibleRange;
		if (o === null) return;
		let { from: s, to: c } = o;
		s >= c || e.useBitmapCoordinateSpace((e) => this.drawImpl(e, {
			data: i,
			options: a,
			priceToCoordinate: t,
			from: s,
			to: c,
			isHovered: n ?? !1,
			hitTestData: r
		}));
	}
};
//#endregion
//#region src/compat.ts
function x(e) {
	let t = e.conflationFactor;
	return e.barSpacing * (t !== void 0 && t > 0 ? t : 1);
}
//#endregion
//#region src/renderer.ts
function S(e) {
	return a[e % a.length];
}
function C(e) {
	let t = 0;
	for (let n of e) Number.isFinite(n) && (t = Math.max(t, Math.abs(n)));
	return t;
}
var w = class extends b {
	_hitAreas = [];
	_allScale = null;
	hitTest(e, t) {
		for (let n of this._hitAreas) if (!(e < n.left || e > n.right || t < n.top || t > n.bottom)) return {
			distance: 0,
			objectId: n.objectId,
			type: "range",
			hitTestData: n.index
		};
		return null;
	}
	drawImpl(e, t) {
		let { data: i, options: a, priceToCoordinate: o, isHovered: s, hitTestData: c } = t;
		this._hitAreas = [];
		let l = o(a.baseValue);
		if (l === null) return;
		let u = n(i, (e, t) => ({
			x: e.x,
			time: e.time,
			index: t,
			values: e.originalData.values,
			colors: e.originalData.colors
		}));
		if (u.length === 0) return;
		let d = a.scaleMode === "price" ? null : this._pixelScale(a, i.bars, u);
		if (a.scaleMode === "pixels" && d === null) return;
		let f = e.horizontalPixelRatio;
		v(u, x(i), f, 0, u.length, r(i));
		let p = a.borderColor === null ? 0 : Math.max(1, Math.round(a.borderWidth * f)), m = s && a.highlightHovered && typeof c == "number" ? c : null, h = e.context;
		h.save();
		for (let t of u) {
			let n = t.column;
			if (n === void 0) continue;
			let r = n.right - n.left + 1, i = Math.max(1, Math.round(r * a.widthPercent / 100));
			h.globalAlpha = m === null || m === t.index ? 1 : .35, this._drawPoint(e, t, a, {
				left: n.left + Math.round((r - i) / 2),
				width: i,
				borderWidth: p,
				baseCoordinate: l,
				halfGap: a.gap / 2,
				scale: d,
				priceToCoordinate: o
			});
		}
		h.restore();
	}
	_pixelScale(e, t, n) {
		if (typeof e.normalize == "number") return e.normalize > 0 ? e.normalize : null;
		if (e.normalize === "all") {
			if (this._allScale === null || this._allScale.bars !== t) {
				let e = 0;
				for (let n of t) e = Math.max(e, C(n.originalData.values));
				this._allScale = {
					bars: t,
					value: e
				};
			}
			return this._allScale.value > 0 ? this._allScale.value : null;
		}
		let r = 0;
		for (let e of n) r = Math.max(r, C(e.values));
		return r > 0 ? r : null;
	}
	_drawPoint(e, t, n, r) {
		let i = e.verticalPixelRatio, { left: a, width: o } = r, s = Infinity, l = -Infinity;
		for (let d = 0; d < t.values.length; d++) {
			let f = t.values[d];
			if (!Number.isFinite(f)) continue;
			let p = this._columnEnd(f, r.baseCoordinate, n, r);
			if (p === null) continue;
			let m = p <= r.baseCoordinate, h = m ? -r.halfGap : r.halfGap, g = y(r.baseCoordinate + h, p + h, i), _ = S(d), v = c(n.borderRadius[_] * e.horizontalPixelRatio, o, g.length), b = m ? [
				v,
				v,
				0,
				0
			] : [
				0,
				0,
				v,
				v
			];
			u(e.context, a, g.position, o, g.length, t.colors?.[d] ?? n.colors[_], r.borderWidth, b, n.borderColor ?? void 0), s = Math.min(s, g.position), l = Math.max(l, g.position + g.length);
		}
		s <= l && this._hitAreas.push({
			index: t.index,
			left: a / e.horizontalPixelRatio,
			right: (a + o) / e.horizontalPixelRatio,
			top: s / i,
			bottom: l / i,
			objectId: `bar-${t.index}`
		});
	}
	_columnEnd(e, t, n, r) {
		if (n.scaleMode === "price") {
			let t = r.priceToCoordinate(n.baseValue + e);
			return t === null ? null : t;
		}
		let i = Math.abs(e) / r.scale * (n.maxHeight / 2);
		return e >= 0 ? t - i : t + i;
	}
};
//#endregion
//#region src/keep-in-view.ts
function T(e, t, n) {
	let r = !1, i = !1, a = () => {
		if (r) return;
		let i = e.panes().find((e) => e.getSeries().some((e) => Object.is(e, t)))?.getHeight() ?? 0;
		if (i <= 0) return;
		let a = n ?? t.options().maxHeight, o = Math.max(0, Math.min(.3, a / 2 / i)), s = t.priceScale(), c = s.options().scaleMargins;
		(c.top !== o || c.bottom !== o) && s.applyOptions({ scaleMargins: {
			top: o,
			bottom: o
		} });
	}, o = {
		updateAllViews: () => {
			r || i || (i = !0, queueMicrotask(() => {
				i = !1, a();
			}));
		},
		detached: () => {
			r = !0;
		}
	};
	return t.attachPrimitive(o), a(), () => {
		r || (r = !0, t.detachPrimitive(o));
	};
}
//#endregion
//#region src/dual-range-histogram-series.ts
var E = class {
	_readOptions;
	_renderer;
	_options = o;
	constructor(e) {
		this._readOptions = e, this._renderer = new w();
	}
	priceValueBuilder(e) {
		let t = this._readOptions?.() ?? this._options, n = t.baseValue;
		if (t.scaleMode !== "price") return [n];
		let r = 0, i = 0;
		for (let t of e.values) Number.isFinite(t) && (r = Math.min(r, t), i = Math.max(i, t));
		return [
			n + r,
			n + i,
			n
		];
	}
	isWhitespace(e) {
		return !e.values?.length;
	}
	renderer() {
		return this._renderer;
	}
	update(e, t) {
		this._options = t, this._renderer.update(e, t);
	}
	conflationReducer(e, t) {
		return t.data;
	}
	defaultOptions() {
		return o;
	}
};
function D(e, t = {}, n = 0) {
	return i(e, (e) => new E(e), o, t, ["scaleMode", "baseValue"], n);
}
//#endregion
export { E as DualRangeHistogramSeries, D as createDualRangeHistogramSeries, o as defaultOptions, T as keepPixelSeriesInView };
