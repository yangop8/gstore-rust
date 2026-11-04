# Phase 6 R4 measure stage, part 2 (R4-SNAP, G4): concurrency, mixed workload, `/api begin` — univ 8

`phase6-r4-conc-lubm8.json` is the first head run of the quiet post-merge stage (`8264302`, nothing else running on the 4 vCPU VM, 2026-11-04): `snapshot`, `disk@stream` (`GSTORE_DISK_QUERY_MODE=stream`) and `disk@materialize` (`=memory`), three runs joined with `gstore-bench merge`. Each ran `target/release/gstore-bench run --profile reference --scale 8 --extra none --backends … --reps 1 --query-procs 1 --open-reps 1 --no-update --conc-clients 1,8 --conc-secs 5 --conc-queries q1,q4 --conc-endpoints sparql,api --mixed-secs 10` under `flock /tmp/gstore-bench.lock`.

The baseline of the first section is the first base run of the same session: `/tmp/gstore-base-d702463/bin` (the tree before Round 4) with its own harness, the same options without the ones it lacks (its harness has no mixed phase and no `/api begin` point, so those rows have no ratio); its two disk runs were given the labels `stream` / `materialize` so that they match these runs (the base files stay out of the tree). The query phase of these runs is one process and one warm repetition: noise-level, not this file's subject (the memory layout's query, build and insert figures are in `changelog.d/phase6-R4-SNAP.md`). The second section compares the concurrency points with pre-Phase-6; a full `compare` against pre fails only its three "the result did not run the update phase" exact checks (`--no-update`).

## Against the base (`d702463`), same session

### gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- commit `82643024c992c2bf4c4198072c6364aafa9f69f4`, linux x86_64 × 4 CPUs, 16091 MiB, load 6.57 4.12 2.32
- calibration score 70.96 ms (sort 43.69, hashmap 188.00, format 43.50, fsync 1.19)
- compared with: reference profile, commit `82643024c992c2bf4c4198072c6364aafa9f69f4`

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.41 | 112.6 | 85.5 | 172.7 | 176.4 | 102.6 | 127.4 | 2.56 | 1.08× | 0.96× | 0.58× |
| disk@materialize | 3.30 | 104.3 | 130.2 | 249.6 | 246.6 | 89.5 | 114.1 | 2.24 | 0.91× | 0.83× | 0.34× |
| disk@stream | 3.26 | 104.2 | 130.2 | 1.3 | 0.8 | 6.7 | 66.2 | 3.71 | 0.85× | 0.97× | 0.38× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 2909, spo 122, index 172, pos 93, osp+vstree 172, dictionary 77, finish 91, install 2 | 1024.0 | 88.0 | 24.1 | 4 | 0 | 0.0 |
| disk@stream | parse+encode 2849, spo 125, index 181, pos 100, osp+vstree 180, dictionary 61, finish 97, install 3 | 1024.0 | 88.0 | 24.1 | 4 | 0 | 0.0 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.24 | 0.02 | 0.16× | 0.21 | 0.02 | 0.04× | 1.48 | 0.08 | 0.08× |
| q2 | 25 | 7.04 | 6.22 | 0.76× | 5.66 | 4.94 | 0.57× | 15.48 | 7.29 | 0.45× |
| q3 | 10 | 0.07 | 0.02 | 0.20× | 0.07 | 0.02 | 0.05× | 0.11 | 0.02 | 0.03× |
| q4 | 9 | 0.09 | 0.06 | 0.68× | 0.13 | 0.07 | 0.32× | 0.16 | 0.08 | 0.16× |
| q5 | 576 | 30.08 | 24.06 | 0.51× | 31.96 | 24.99 | 0.60× | 37.55 | 27.70 | 0.42× |
| q6 | 80056 | 14.82 | 13.80 | 0.55× | 17.37 | 17.06 | 0.59× | 51.44 | 35.12 | 0.57× |
| q7 | 23 | 28.67 | 30.14 | 0.40× | 29.42 | 27.49 | 0.47× | 41.21 | 33.13 | 0.51× |
| q8 | 10778 | 35.11 | 32.05 | 0.82× | 32.40 | 31.29 | 0.57× | 51.61 | 46.42 | 0.69× |
| q9 | 1081 | 43.53 | 47.55 | 0.92× | 44.70 | 44.37 | 0.61× | 79.31 | 65.84 | 0.66× |
| q10 | 3 | 22.63 | 21.97 | 0.75× | 23.45 | 22.83 | 0.55× | 28.35 | 27.71 | 0.64× |
| q11 | 325 | 0.74 | 0.27 | 0.97× | 0.41 | 0.16 | 0.38× | 0.74 | 0.22 | 0.44× |
| q12 | 21 | 0.37 | 0.25 | 1.08× | 0.11 | 0.05 | 0.28× | 0.38 | 0.18 | 0.89× |
| q13 | 85670 | 18.20 | 15.80 | 0.63× | 18.74 | 18.65 | 0.43× | 39.50 | 34.13 | 0.68× |
| q14 | 60576 | 10.34 | 10.43 | 0.69× | 12.44 | 12.26 | 0.58× | 31.04 | 21.86 | 0.79× |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 12152.0 | 0.08 | 0.12 | 0 | identical | 1.28× |
| snapshot | sparql | q1 | 8 | 74445.6 | 0.10 | 0.27 | 0 | identical | 1.39× |
| snapshot | sparql | q4 | 1 | 6502.8 | 0.15 | 0.25 | 0 | identical | 1.11× |
| snapshot | sparql | q4 | 8 | 28544.4 | 0.27 | 0.64 | 0 | identical | 0.88× |
| snapshot | api | q1 | 1 | 5809.3 | 0.16 | 0.36 | 0 | identical | 1.24× |
| snapshot | api | q1 | 8 | 19196.6 | 0.40 | 0.89 | 0 | identical | 1.27× |
| snapshot | api | q4 | 1 | 2840.0 | 0.31 | 0.60 | 0 | identical | 1.16× |
| snapshot | api | q4 | 8 | 11542.5 | 0.65 | 1.43 | 0 | identical | 1.11× |
| disk@materialize | sparql | q1 | 1 | 11614.3 | 0.08 | 0.13 | 0 | identical | 7.48× |
| disk@materialize | sparql | q1 | 8 | 68440.9 | 0.11 | 0.30 | 0 | identical | 21.09× |
| disk@materialize | sparql | q4 | 1 | 5560.2 | 0.18 | 0.29 | 0 | identical | 2.49× |
| disk@materialize | sparql | q4 | 8 | 30529.3 | 0.26 | 0.57 | 0 | identical | 4.89× |
| disk@materialize | api | q1 | 1 | 5541.2 | 0.17 | 0.33 | 0 | identical | 4.08× |
| disk@materialize | api | q1 | 8 | 16991.4 | 0.46 | 1.03 | 0 | identical | 6.92× |
| disk@materialize | api | q4 | 1 | 2592.0 | 0.34 | 0.60 | 0 | identical | 1.75× |
| disk@materialize | api | q4 | 8 | 10690.8 | 0.70 | 1.56 | 0 | identical | 2.47× |
| disk@stream | sparql | q1 | 1 | 11414.9 | 0.09 | 0.13 | 0 | identical | 8.01× |
| disk@stream | sparql | q1 | 8 | 46920.4 | 0.16 | 0.39 | 0 | identical | 12.90× |
| disk@stream | sparql | q4 | 1 | 4715.4 | 0.21 | 0.40 | 0 | identical | 2.47× |
| disk@stream | sparql | q4 | 8 | 21534.1 | 0.37 | 0.76 | 0 | identical | 3.50× |
| disk@stream | api | q1 | 1 | 6027.7 | 0.16 | 0.30 | 0 | identical | 5.09× |
| disk@stream | api | q1 | 8 | 15449.8 | 0.50 | 1.14 | 0 | identical | 4.95× |
| disk@stream | api | q4 | 1 | 2440.8 | 0.37 | 0.70 | 0 | identical | 1.77× |
| disk@stream | api | q4 | 8 | 10329.4 | 0.73 | 1.56 | 0 | identical | 2.27× |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | off | q4 | 8 | 32953.2 | 0.23 | 0.52 | 101 / 0 | 0.67 / 1.48 | identical | – |
| snapshot | sparql | on | q4 | 8 | 67.7 | 118.06 | 311.13 | 37 / 0 | 277.80 / 314.61 | identical | – |
| snapshot | api | off | q4 | 8 | 12038.1 | 0.60 | 1.32 | 101 / 0 | 1.09 / 1.94 | identical | – |
| snapshot | api | on | q4 | 8 | 34.5 | 226.67 | 318.23 | 39 / 0 | 277.59 / 320.47 | identical | – |
| disk@materialize | sparql | off | q4 | 8 | 31785.3 | 0.24 | 0.54 | 101 / 0 | 3.60 / 5.36 | identical | – |
| disk@materialize | sparql | on | q4 | 8 | 31403.7 | 0.25 | 0.52 | 101 / 0 | 5.62 / 4.69 | identical | – |
| disk@materialize | api | off | q4 | 8 | 11014.1 | 0.66 | 1.53 | 101 / 0 | 4.28 / 6.09 | identical | – |
| disk@materialize | api | on | q4 | 8 | 11119.0 | 0.65 | 1.55 | 101 / 0 | 4.30 / 6.16 | identical | – |
| disk@stream | sparql | off | q4 | 8 | 22766.7 | 0.34 | 0.73 | 101 / 0 | 3.79 / 5.32 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 21801.4 | 0.36 | 0.73 | 101 / 0 | 7.52 / 5.26 | identical | – |
| disk@stream | api | off | q4 | 8 | 10171.7 | 0.72 | 1.65 | 101 / 0 | 4.28 / 6.11 | identical | – |
| disk@stream | api | on | q4 | 8 | 10177.6 | 0.72 | 1.63 | 101 / 0 | 4.01 / 5.53 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 20 | 0.37 | 0.60 | 1.31 | 0.1 / 0.1 | – |
| disk@materialize | 20 | 0.29 | 0.55 | 0.74 | 0.0 / 0.0 | – |
| disk@stream | 20 | 0.31 | 0.49 | 0.53 | 0.1 / 0.1 | – |

#### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.13 | 0.02 | 0.95 | 3.00 |  |
| pass | time.query | snapshot/q2 | 8.16 | 5.53 | 0.74 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.09 | 0.02 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.09 | 0.05 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q5 | 47.43 | 21.41 | 0.47 | 3.00 |  |
| pass | time.query | snapshot/q6 | 25.26 | 12.27 | 0.52 | 3.00 |  |
| pass | time.query | snapshot/q7 | 74.97 | 26.81 | 0.37 | 3.00 |  |
| pass | time.query | snapshot/q8 | 39.01 | 28.51 | 0.74 | 3.00 |  |
| pass | time.query | snapshot/q9 | 51.56 | 42.30 | 0.83 | 3.00 |  |
| pass | time.query | snapshot/q10 | 29.41 | 19.55 | 0.69 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.27 | 0.24 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.23 | 0.22 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q13 | 25.24 | 14.05 | 0.59 | 3.00 |  |
| pass | time.query | snapshot/q14 | 15.21 | 9.28 | 0.66 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.58 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | snapshot | 5010.19 | 4812.67 | 0.96 | 2.00 |  |
| pass | time.open | snapshot | 183.04 | 156.92 | 0.87 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 0.48 | 0.01 | 0.81 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 8.63 | 4.00 | 0.56 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 0.41 | 0.02 | 0.84 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 0.21 | 0.05 | 0.93 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 41.77 | 20.23 | 0.51 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 28.70 | 13.81 | 0.51 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 58.26 | 22.25 | 0.40 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 54.58 | 25.32 | 0.48 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 72.50 | 35.91 | 0.51 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 41.53 | 18.48 | 0.47 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 0.43 | 0.13 | 0.88 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 0.19 | 0.04 | 0.93 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 43.07 | 15.09 | 0.38 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 21.05 | 9.92 | 0.52 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.45 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk@materialize | 3640.10 | 2673.14 | 0.74 | 2.50 |  |
| pass | time.open | disk@materialize | 295.67 | 199.59 | 0.70 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 0.96 | 0.08 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 16.21 | 7.37 | 0.51 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 0.89 | 0.02 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 0.51 | 0.08 | 0.83 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 66.04 | 27.98 | 0.44 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 61.52 | 35.48 | 0.59 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 65.53 | 33.48 | 0.53 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 67.69 | 46.90 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 99.03 | 66.52 | 0.68 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 43.25 | 27.99 | 0.66 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 0.50 | 0.22 | 0.89 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.20 | 0.18 | 0.99 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 50.22 | 34.49 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 27.62 | 22.09 | 0.81 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.60 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk@stream | 3822.67 | 3294.81 | 0.87 | 2.50 |  |
| pass | time.open | disk@stream | 0.81 | 0.79 | 1.00 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 126.38 | 127.40 | 1.01 | 1.50 |  |
| pass | rss.build_peak | snapshot | 112.70 | 112.64 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 102.23 | 102.64 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 117.38 | 114.05 | 0.98 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 107.21 | 104.31 | 0.98 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 89.15 | 89.51 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 69.32 | 66.17 | 0.97 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 97.19 | 104.19 | 1.05 | 1.50 |  |
| pass | rss.after_open | disk@stream | 6.01 | 6.70 | 1.02 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 130150406.00 | 130150400.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@stream | 130150406.00 | 130150400.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 9522.81 | 12152.02 | 1.28 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 53583.72 | 74445.64 | 1.39 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 5834.14 | 6502.76 | 1.11 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 32327.66 | 28544.41 | 0.88 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 4679.95 | 5809.30 | 1.24 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 15131.49 | 19196.64 | 1.27 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2453.81 | 2840.02 | 1.16 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 10389.60 | 11542.48 | 1.11 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 1553.06 | 11614.34 | 7.48 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 3244.80 | 68440.94 | 21.09 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 2230.87 | 5560.21 | 2.49 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 6245.25 | 30529.33 | 4.89 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 1358.40 | 5541.21 | 4.08 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 2454.05 | 16991.42 | 6.92 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 1481.86 | 2592.02 | 1.75 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 4327.90 | 10690.75 | 2.47 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 1425.61 | 11414.90 | 8.01 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 3637.27 | 46920.36 | 12.90 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 1912.45 | 4715.36 | 2.47 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 6150.36 | 21534.11 | 3.50 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 1183.80 | 6027.75 | 5.09 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 3122.00 | 15449.84 | 4.95 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 1376.23 | 2440.79 | 1.77 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 4550.16 | 10329.39 | 2.27 | – | report-only |


## Concurrency against pre-Phase-6 (`pre-phase6-lubm8.json`)

| workload | backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---|---:|
| lubm-like univ 8 | snapshot | sparql | q1 | 1 | 12152.0 | 0.08 | 0.12 | 0 | identical | 6.07× |
| lubm-like univ 8 | snapshot | sparql | q1 | 8 | 74445.6 | 0.10 | 0.27 | 0 | identical | 35.46× |
| lubm-like univ 8 | snapshot | sparql | q4 | 1 | 6502.8 | 0.15 | 0.25 | 0 | identical | 80.25× |
| lubm-like univ 8 | snapshot | sparql | q4 | 8 | 28544.4 | 0.27 | 0.64 | 0 | identical | 373.94× |
| lubm-like univ 8 | snapshot | api | q1 | 1 | 5809.3 | 0.16 | 0.36 | 0 | identical | 3.59× |
| lubm-like univ 8 | snapshot | api | q1 | 8 | 19196.6 | 0.40 | 0.89 | 0 | identical | 8.37× |
| lubm-like univ 8 | snapshot | api | q4 | 1 | 2840.0 | 0.31 | 0.60 | 0 | identical | 33.17× |
| lubm-like univ 8 | snapshot | api | q4 | 8 | 11542.5 | 0.65 | 1.43 | 0 | identical | 150.83× |
| lubm-like univ 8 | disk@materialize | sparql | q1 | 1 | 11614.3 | 0.08 | 0.13 | 0 | identical | 7.53× |
| lubm-like univ 8 | disk@materialize | sparql | q1 | 8 | 68440.9 | 0.11 | 0.30 | 0 | identical | 47.22× |
| lubm-like univ 8 | disk@materialize | sparql | q4 | 1 | 5560.2 | 0.18 | 0.29 | 0 | identical | 65.51× |
| lubm-like univ 8 | disk@materialize | sparql | q4 | 8 | 30529.3 | 0.26 | 0.57 | 0 | identical | 385.05× |
| lubm-like univ 8 | disk@materialize | api | q1 | 1 | 5541.2 | 0.17 | 0.33 | 0 | identical | 4.57× |
| lubm-like univ 8 | disk@materialize | api | q1 | 8 | 16991.4 | 0.46 | 1.03 | 0 | identical | 11.63× |
| lubm-like univ 8 | disk@materialize | api | q4 | 1 | 2592.0 | 0.34 | 0.60 | 0 | identical | 30.18× |
| lubm-like univ 8 | disk@materialize | api | q4 | 8 | 10690.8 | 0.70 | 1.56 | 0 | identical | 139.87× |
| lubm-like univ 8 | disk@stream | sparql | q1 | 1 | 11414.9 | 0.09 | 0.13 | 0 | identical | 7.40× |
| lubm-like univ 8 | disk@stream | sparql | q1 | 8 | 46920.4 | 0.16 | 0.39 | 0 | identical | 32.37× |
| lubm-like univ 8 | disk@stream | sparql | q4 | 1 | 4715.4 | 0.21 | 0.40 | 0 | identical | 55.56× |
| lubm-like univ 8 | disk@stream | sparql | q4 | 8 | 21534.1 | 0.37 | 0.76 | 0 | identical | 271.60× |
| lubm-like univ 8 | disk@stream | api | q1 | 1 | 6027.7 | 0.16 | 0.30 | 0 | identical | 4.97× |
| lubm-like univ 8 | disk@stream | api | q1 | 8 | 15449.8 | 0.50 | 1.14 | 0 | identical | 10.58× |
| lubm-like univ 8 | disk@stream | api | q4 | 1 | 2440.8 | 0.37 | 0.70 | 0 | identical | 28.42× |
| lubm-like univ 8 | disk@stream | api | q4 | 8 | 10329.4 | 0.73 | 1.56 | 0 | identical | 135.14× |


Mixed workload and `/api begin` (pre-Phase-6 has neither):

| workload | backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| lubm-like univ 8 | snapshot | sparql | off | q4 | 8 | 32953.2 | 0.23 | 0.52 | 101 / 0 | 0.67 / 1.48 | identical | – |
| lubm-like univ 8 | snapshot | sparql | on | q4 | 8 | 67.7 | 118.06 | 311.13 | 37 / 0 | 277.80 / 314.61 | identical | – |
| lubm-like univ 8 | snapshot | api | off | q4 | 8 | 12038.1 | 0.60 | 1.32 | 101 / 0 | 1.09 / 1.94 | identical | – |
| lubm-like univ 8 | snapshot | api | on | q4 | 8 | 34.5 | 226.67 | 318.23 | 39 / 0 | 277.59 / 320.47 | identical | – |
| lubm-like univ 8 | disk@materialize | sparql | off | q4 | 8 | 31785.3 | 0.24 | 0.54 | 101 / 0 | 3.60 / 5.36 | identical | – |
| lubm-like univ 8 | disk@materialize | sparql | on | q4 | 8 | 31403.7 | 0.25 | 0.52 | 101 / 0 | 5.62 / 4.69 | identical | – |
| lubm-like univ 8 | disk@materialize | api | off | q4 | 8 | 11014.1 | 0.66 | 1.53 | 101 / 0 | 4.28 / 6.09 | identical | – |
| lubm-like univ 8 | disk@materialize | api | on | q4 | 8 | 11119.0 | 0.65 | 1.55 | 101 / 0 | 4.30 / 6.16 | identical | – |
| lubm-like univ 8 | disk@stream | sparql | off | q4 | 8 | 22766.7 | 0.34 | 0.73 | 101 / 0 | 3.79 / 5.32 | identical | – |
| lubm-like univ 8 | disk@stream | sparql | on | q4 | 8 | 21801.4 | 0.36 | 0.73 | 101 / 0 | 7.52 / 5.26 | identical | – |
| lubm-like univ 8 | disk@stream | api | off | q4 | 8 | 10171.7 | 0.72 | 1.65 | 101 / 0 | 4.28 / 6.11 | identical | – |
| lubm-like univ 8 | disk@stream | api | on | q4 | 8 | 10177.6 | 0.72 | 1.63 | 101 / 0 | 4.01 / 5.53 | identical | – |

| workload | backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---|---:|---:|---:|---:|---:|---:|
| lubm-like univ 8 | snapshot | 20 | 0.37 | 0.60 | 1.31 | 0.1 / 0.1 | – |
| lubm-like univ 8 | disk@materialize | 20 | 0.29 | 0.55 | 0.74 | 0.0 / 0.0 | – |
| lubm-like univ 8 | disk@stream | 20 | 0.31 | 0.49 | 0.53 | 0.1 / 0.1 | – |

