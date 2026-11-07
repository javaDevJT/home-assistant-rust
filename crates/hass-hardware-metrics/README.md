# Native Hardware metrics

Linux proc and macOS Mach collectors implement the CPU and available-memory
behavior audited from psutil 7.2.2. `NativeMetricsProvider::new()` performs no
OS reads. `MetricsProvider::read()` returns concrete errors, keeps a serialized
CPU baseline, takes two immediate CPU samples on its first read, and supplies
typed values to the app's shared Hardware sampler.

Linux reads aggregate CPU counters, meminfo and zone watermarks, including the
missing-or-zero MemAvailable fallback and source clamps. macOS reads Mach CPU
ticks, `hw.memsize` and VM pages; available memory is inactive plus free pages.
Other platforms return an explicit unsupported error. Percentage quantization
preserves Python's binary-input decimal rounding before the API's integer CPU
rounding. Missing or failed measurements do not become successful zeros.

```sh
cargo test -p hass-hardware-metrics --locked --offline
```

The 15 tests use fixture strings and injected counters. They do not read host
metrics. Actual native collection remains unverified; compilation, synthetic
tests and an idle app owner do not prove physical measurements. See the
[source contract](../../docs/HARDWARE-PORT.md) and retained
[BSD-3 attribution](THIRD_PARTY_NOTICES.md).
