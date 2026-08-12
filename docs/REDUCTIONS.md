# Native temporal and diurnal reductions

Rusty Weather treats temporal analytics as a first-class data operation. A
reduction is evaluated from one immutable run snapshot and an exact ordered
valid-time axis. It never combines initialization cycles implicitly.

## Time windows

Requested UTC and resolved local-day windows are half-open physical intervals:
[start, end). Sample selection follows each field's declared support. For
example, an hourly interval stamped at its ending valid time selects labels in
(start, end], so the interval ending at local midnight is included and the
one ending at the opening boundary is not. Cumulative fields read the immediate
pre-window value as a differencing baseline without exposing it as an in-window
sample.

Local-day windows require an IANA timezone and are resolved to explicit UTC
boundaries before any data is read. This means daylight-saving transitions
produce honest 23-hour or 25-hour days. Every response includes both the
requested local interval and the resolved UTC interval.

Legacy whole-hour runs use their run origin plus forecast lead. Exact-time
runs use the persisted `lead_seconds` and `valid_unix` values and therefore
support subhourly and irregular output.

## Missing data

Strict mode is the default. A missing expected valid time blocks the result
and names the gap. Partial mode must be requested explicitly and reports:

- expected and available sample counts;
- expected and covered duration;
- missing valid times and the largest gap;
- per-cell finite sample count when requested;
- the basis used to determine expectations.

Non-finite values are missing observations. Positive and negative infinity
are invalid data, not legitimate extrema. A cell with no finite observations
is returned as missing.

## Reducer semantics

The query request must declare `TemporalSemantics`; the engine does not infer
it from a variable name or unit string. Results echo the declaration and the
validated reducer. `unknown` fields remain raw-sampling only.

Operations depend on the physical temporal meaning of a variable:

| Semantics | Default operations |
| --- | --- |
| Instantaneous continuous scalar | min, max, range, time-weighted mean, argmin time, argmax time |
| Interval accumulation | sum, min/max/range of interval amount, argmin/argmax time |
| Fixed-window maximum | min/max/range across interval maxima, argmin/argmax time; never sum or instantaneous mean |
| Cumulative from run origin | reset-aware differencing to increments, then total/min/max/range of increments |
| Interval mean or rate | min/max/range rate, duration-weighted mean, and a physical integral when defined |
| Vector pair | speed extrema/range, mean speed, vector-mean components/speed/direction |
| Circular direction | circular mean; scalar min/max/range are rejected |
| Categorical | mode, duration by category, transitions; numeric reducers are rejected |
| Unknown | raw sampling only until semantics are declared |

Every range is `finite_max - finite_min` in the same physical quantity and
units as its extrema. The response names preserve that quantity:
`range_interval`, `range_increment`, `range_rate`, `range_speed`,
`range_of_interval_maxima`, or scalar `range`. A range is missing unless both
finite extrema exist. Extrema ties select the earliest exact valid time.
Arg-time grids store integer indices into an accompanying exact time
coordinate; Unix timestamps are never coerced into lossy `f32` values.

Instantaneous, vector, circular, and categorical duration weighting uses a
left-constant support interval from each expected timestamp to the next
expected timestamp (or the half-open window end). Interval fields instead
declare whether support starts or ends at valid time, or follows adjacent
expected times. Accumulation intervals crossing the query boundary are not
silently prorated, and overlapping interval supports are rejected rather than
double-counted. Rate integrals require an explicit seconds-per-rate-unit
conversion and explicit output unit.

Fixed-window maximum samples (for example `*_max_1h`) use the separate
`interval_maximum` semantics and `interval_maximum_summary` reducer. The
result names each field as an operation on interval maxima and reports finite
sample count plus union-of-support duration coverage. Overlapping trailing
windows therefore cannot inflate coverage above 100 percent. A fixed window
crossing the query boundary is excluded rather than relabeled as an in-window
maximum.

## Spatial behavior

Temporal and spatial reductions are distinct. A temporal-grid operation
reduces every native-domain cell across time. A spatial-series operation
returns precomputed full-domain finite minimum, maximum, and counts at each
valid time without decoding field payloads.

POST /v1/window reads one bounded, half-open native index rectangle from one
storage slot. POST /v1/geographic-window instead resolves a finite geographic
bbox against an exact `snapshot_id` and `grid_hash`. Longitude is the eastward
arc from west to east, so west > east crosses the antimeridian and -180..180
selects the full globe. The response is the minimal native rectangular
envelope containing selected grid-point centres, with only that envelope's
lat/lon arrays, exact projection metadata, and an explicit per-cell mask.
Surface fields and explicit pressure levels use window/chunk reads; pressure
levels remain separate and are never vertically reduced. Polygon aggregation
and reprojection remain outside this contract.

## Determinism and memory

Samples for a cell are combined in chronological order. Input fields are
decoded tile by tile, so the engine never retains every
domain_cells * timesteps * variables input value at once. Result memory still
scales with domain cells and the reducer's output fields; the query limits
reject the reducer-specific output cardinality before allocation.
Synchronous JSON is separately capped by `json_grid_values` and
`sync_result_values`. Geographic windows have independent
`geographic_window_cells` and `geographic_window_output_values` caps.
Asynchronous temporal jobs use
`temporal_reduction_cells` and `temporal_output_values`; the production
defaults admit full-domain HRRR scalar and 13-array vector summaries while retaining
the independent serialized `job_result_bytes` cap.

The conformance suite covers irregular cadence, NaNs, ties, missing samples,
cumulative baselines and resets, interval boundary labels, vector and
categorical rejection, and 23/24/25-hour local-day boundaries.
