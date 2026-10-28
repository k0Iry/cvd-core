# Vendored frontend dependencies

These files are unmodified release artifacts fetched from the official npm registry. The application uses an import map so the plugin and application resolve one shared `lightweight-charts` ES-module identity.

## lightweight-charts 5.1.0

- Package: `lightweight-charts@5.1.0`
- Tarball: https://registry.npmjs.org/lightweight-charts/-/lightweight-charts-5.1.0.tgz
- Tarball SHA-256: `15af2ec27e4ac09b04bd276674548a2dca368a6e6dd94d6f29809fb67156ed39`
- Artifact: `lightweight-charts-5.1.0/lightweight-charts.standalone.production.mjs`
- Artifact SHA-256: `896aa8f75f7d5a5d083cf50961ab44f4f9d3e098cf24d18d8cae78f656aef56f`
- License: Apache-2.0; see `lightweight-charts-5.1.0/LICENSE`

The standalone ESM build includes its runtime dependency; it does not create a `window.LightweightCharts` global.

## Dual range histogram series 1.0.0

- Package: `@tradingview/lwc-plugin-dual-range-histogram-series@1.0.0`
- Tarball: https://registry.npmjs.org/@tradingview/lwc-plugin-dual-range-histogram-series/-/lwc-plugin-dual-range-histogram-series-1.0.0.tgz
- Tarball SHA-256: `fa4efee077f3f0a9b671b6f22866d3ac25e6e3fe8f7e1c30787e0548f4e8f4a9`
- Artifact: `lwc-plugin-dual-range-histogram-series-1.0.0/dual-range-histogram-series.standalone.js`
- Artifact SHA-256: `78226bca730a0d8950825bcc256f312a6a0caadbc6caf3fabad61da2bdac8865`
- Source release commit reviewed: https://github.com/tradingview/lightweight-charts/tree/bf1ac13ac434d813b14096b558398aa615211923/packages/lwc-plugin-dual-range-histogram-series
- License and notices: Apache-2.0; see the package `LICENSE` and `NOTICE` files.

Despite its filename, the plugin standalone artifact is ESM. It externalizes the bare `lightweight-charts` import while bundling its other runtime helpers.
