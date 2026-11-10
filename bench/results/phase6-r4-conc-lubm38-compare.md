# Phase 6 R4 measure stage, part 2 (R4-SNAP, G4): concurrency, mixed workload, `/api begin` — univ 38

`phase6-r4-conc-lubm38.json` is the first head run of the quiet post-merge stage (`8264302`, nothing else running on the 4 vCPU VM, 2026-11-04): `snapshot`, `disk@stream` (`GSTORE_DISK_QUERY_MODE=stream`) and `disk@materialize` (`=memory`), three runs joined with `gstore-bench merge`. Each ran `target/release/gstore-bench run --profile reference --scale 38 --extra none --backends … --reps 1 --query-procs 1 --open-reps 1 --no-update --conc-clients 1,8 --conc-secs 5 --conc-queries q1,q4 --conc-endpoints sparql,api --mixed-secs 10` under `flock /tmp/gstore-bench.lock`.

The baseline of the first section is the first base run of the same session: `/tmp/gstore-base-d702463/bin` (the tree before Round 4) with its own harness, the same options without the ones it lacks (its harness has no mixed phase and no `/api begin` point, so those rows have no ratio); its two disk runs were given the labels `stream` / `materialize` so that they match these runs (the base files stay out of the tree). The query phase of these runs is one process and one warm repetition: noise-level, not this file's subject (the memory layout's query, build and insert figures were in R4-SNAP's change fragment, folded into `CHANGELOG.md`'s Phase 6 section at the Phase 6 closure; its full text is in the git history of `changelog.d/`). The second section compares the concurrency points with pre-Phase-6; a full `compare` against pre fails only its three "the result did not run the update phase" exact checks (`--no-update`).

## Against the base (`d702463`), same session

### gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- commit `82643024c992c2bf4c4198072c6364aafa9f69f4`, linux x86_64 × 4 CPUs, 16091 MiB, load 6.39 4.69 4.19
- calibration score 84.37 ms (sort 45.10, hashmap 252.83, format 52.66, fsync 0.49)
- compared with: reference profile, commit `82643024c992c2bf4c4198072c6364aafa9f69f4`

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 27.09 | 550.1 | 430.8 | 1472.0 | 982.0 | 489.8 | 583.9 | 14.76 | 0.99× | 0.78× | 0.57× |
| disk@materialize | 19.87 | 434.2 | 654.0 | 1422.0 | 1283.5 | 351.5 | 451.7 | 8.07 | 0.96× | 0.70× | 0.16× |
| disk@stream | 18.59 | 396.3 | 654.0 | 1.0 | 1.1 | 6.7 | 207.3 | 17.16 | 0.90× | 1.37× | 0.28× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 17058, spo 780, index 1582, pos 698, osp+vstree 1582, dictionary 371, finish 432, install 3 | 1024.0 | 384.2 | 155.0 | 4 | 0 | 82.7 |
| disk@stream | parse+encode 16191, spo 661, index 1288, pos 425, osp+vstree 1288, dictionary 400, finish 425, install 3 | 1024.0 | 352.2 | 155.0 | 4 | 0 | 82.7 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.20 | 0.02 | 0.08× | 0.21 | 0.02 | 0.00× | 0.43 | 0.06 | 0.01× |
| q2 | 115 | 70.08 | 58.67 | 1.10× | 32.15 | 28.43 | 0.56× | 115.50 | 99.76 | 1.41× |
| q3 | 10 | 0.14 | 0.05 | 0.06× | 0.11 | 0.02 | 0.00× | 0.21 | 0.03 | 0.01× |
| q4 | 9 | 0.15 | 0.12 | 0.74× | 0.14 | 0.06 | 0.02× | 0.32 | 0.14 | 0.05× |
| q5 | 576 | 561.86 | 318.10 | 0.85× | 324.23 | 216.38 | 0.58× | 349.09 | 294.04 | 0.73× |
| q6 | 404399 | 166.86 | 134.03 | 0.86× | 110.64 | 90.24 | 0.60× | 286.72 | 226.89 | 0.99× |
| q7 | 23 | 540.66 | 480.51 | 1.42× | 236.92 | 229.06 | 0.54× | 350.26 | 291.62 | 0.58× |
| q8 | 10778 | 297.97 | 336.51 | 1.07× | 213.00 | 218.50 | 0.59× | 378.59 | 295.16 | 0.67× |
| q9 | 5507 | 407.29 | 338.16 | 0.76× | 302.73 | 303.01 | 0.47× | 499.28 | 457.95 | 0.69× |
| q10 | 3 | 228.99 | 239.85 | 0.82× | 195.09 | 192.64 | 0.47× | 476.29 | 386.21 | 0.83× |
| q11 | 325 | 2.79 | 1.52 | 0.88× | 0.48 | 0.17 | 0.11× | 1.27 | 0.37 | 0.15× |
| q12 | 21 | 1.19 | 0.92 | 0.83× | 0.17 | 0.09 | 0.06× | 1.21 | 0.36 | 0.18× |
| q13 | 432512 | 93.99 | 90.98 | 0.45× | 99.38 | 103.56 | 0.38× | 240.55 | 240.30 | 0.74× |
| q14 | 305503 | 61.75 | 61.22 | 0.48× | 67.99 | 67.61 | 0.49× | 178.79 | 194.79 | 1.17× |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 12900.3 | 0.08 | 0.11 | 0 | identical | 3.52× |
| snapshot | sparql | q1 | 8 | 73437.6 | 0.10 | 0.27 | 0 | identical | 3.78× |
| snapshot | sparql | q4 | 1 | 6590.7 | 0.15 | 0.24 | 0 | identical | 1.75× |
| snapshot | sparql | q4 | 8 | 32981.3 | 0.24 | 0.53 | 0 | identical | 1.42× |
| snapshot | api | q1 | 1 | 5009.8 | 0.19 | 0.40 | 0 | identical | 2.36× |
| snapshot | api | q1 | 8 | 13468.7 | 0.58 | 1.34 | 0 | identical | 1.54× |
| snapshot | api | q4 | 1 | 2529.7 | 0.35 | 0.67 | 0 | identical | 1.23× |
| snapshot | api | q4 | 8 | 9862.0 | 0.76 | 1.71 | 0 | identical | 1.16× |
| disk@materialize | sparql | q1 | 1 | 12897.4 | 0.08 | 0.11 | 0 | identical | 69.52× |
| disk@materialize | sparql | q1 | 8 | 73645.7 | 0.10 | 0.27 | 0 | identical | 266.61× |
| disk@materialize | sparql | q4 | 1 | 6095.7 | 0.16 | 0.29 | 0 | identical | 17.54× |
| disk@materialize | sparql | q4 | 8 | 35041.2 | 0.22 | 0.48 | 0 | identical | 58.77× |
| disk@materialize | api | q1 | 1 | 5808.4 | 0.16 | 0.35 | 0 | identical | 35.85× |
| disk@materialize | api | q1 | 8 | 17040.4 | 0.46 | 1.02 | 0 | identical | 58.16× |
| disk@materialize | api | q4 | 1 | 2960.9 | 0.30 | 0.57 | 0 | identical | 8.87× |
| disk@materialize | api | q4 | 8 | 11274.3 | 0.66 | 1.45 | 0 | identical | 18.01× |
| disk@stream | sparql | q1 | 1 | 11196.7 | 0.09 | 0.13 | 0 | identical | 71.50× |
| disk@stream | sparql | q1 | 8 | 45279.1 | 0.17 | 0.42 | 0 | identical | 161.62× |
| disk@stream | sparql | q4 | 1 | 4711.4 | 0.21 | 0.38 | 0 | identical | 13.48× |
| disk@stream | sparql | q4 | 8 | 20486.5 | 0.38 | 0.83 | 0 | identical | 34.52× |
| disk@stream | api | q1 | 1 | 5761.8 | 0.16 | 0.31 | 0 | identical | 35.04× |
| disk@stream | api | q1 | 8 | 17196.3 | 0.45 | 1.01 | 0 | identical | 63.41× |
| disk@stream | api | q4 | 1 | 2535.3 | 0.35 | 0.62 | 0 | identical | 8.08× |
| disk@stream | api | q4 | 8 | 9926.4 | 0.76 | 1.71 | 0 | identical | 17.00× |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | off | q4 | 8 | 29837.0 | 0.26 | 0.61 | 101 / 0 | 0.94 / 1.57 | identical | – |
| snapshot | sparql | on | q4 | 8 | 19.5 | 411.20 | 1363.71 | 9 / 0 | 1313.58 / 1629.48 | identical | – |
| snapshot | api | off | q4 | 8 | 9341.5 | 0.78 | 1.72 | 101 / 0 | 2.18 / 2.58 | identical | – |
| snapshot | api | on | q4 | 8 | 7.7 | 1042.62 | 2117.94 | 9 / 0 | 1334.50 / 2118.67 | identical | – |
| disk@materialize | sparql | off | q4 | 8 | 33585.1 | 0.23 | 0.47 | 101 / 0 | 5.81 / 5.23 | identical | – |
| disk@materialize | sparql | on | q4 | 8 | 33723.0 | 0.23 | 0.48 | 101 / 0 | 4.11 / 6.39 | identical | – |
| disk@materialize | api | off | q4 | 8 | 11411.1 | 0.64 | 1.42 | 101 / 0 | 4.30 / 6.19 | identical | – |
| disk@materialize | api | on | q4 | 8 | 10925.6 | 0.67 | 1.52 | 101 / 0 | 4.37 / 5.97 | identical | – |
| disk@stream | sparql | off | q4 | 8 | 20838.3 | 0.38 | 0.82 | 101 / 0 | 3.94 / 5.45 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 21122.9 | 0.37 | 0.76 | 101 / 0 | 6.22 / 5.91 | identical | – |
| disk@stream | api | off | q4 | 8 | 9676.5 | 0.76 | 1.73 | 101 / 0 | 5.81 / 6.13 | identical | – |
| disk@stream | api | on | q4 | 8 | 9407.4 | 0.78 | 1.78 | 101 / 0 | 4.05 / 5.41 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 20 | 0.32 | 0.54 | 0.69 | 0.0 / 0.0 | – |
| disk@materialize | 20 | 0.28 | 0.51 | 0.73 | 0.0 / 0.0 | – |
| disk@stream | 20 | 0.37 | 0.54 | 0.90 | 0.1 / 0.1 | – |

#### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.21 | 0.01 | 0.91 | 3.00 |  |
| pass | time.query | snapshot/q2 | 53.46 | 40.81 | 0.77 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.80 | 0.03 | 0.73 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.16 | 0.08 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q5 | 375.95 | 221.28 | 0.59 | 3.00 |  |
| pass | time.query | snapshot/q6 | 155.84 | 93.23 | 0.60 | 3.00 |  |
| pass | time.query | snapshot/q7 | 338.19 | 334.25 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q8 | 315.59 | 234.08 | 0.74 | 3.00 |  |
| pass | time.query | snapshot/q9 | 445.28 | 235.23 | 0.53 | 3.00 |  |
| pass | time.query | snapshot/q10 | 292.09 | 166.85 | 0.57 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.74 | 1.06 | 0.82 | 3.00 |  |
| pass | time.query | snapshot/q12 | 1.11 | 0.64 | 0.85 | 3.00 |  |
| pass | time.query | snapshot/q13 | 203.68 | 63.29 | 0.32 | 3.00 |  |
| pass | time.query | snapshot/q14 | 126.28 | 42.59 | 0.35 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.57 | 1.50 | geomean of 11 warm-median ratios |
| pass | time.build | snapshot | 27410.88 | 18843.07 | 0.69 | 2.00 |  |
| pass | time.open | snapshot | 1261.64 | 683.08 | 0.55 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 5.58 | 0.02 | 0.27 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 50.86 | 23.29 | 0.48 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 5.74 | 0.02 | 0.26 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 2.76 | 0.05 | 0.43 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 375.03 | 177.24 | 0.48 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 150.37 | 73.92 | 0.50 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 421.65 | 187.63 | 0.45 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 371.52 | 178.98 | 0.48 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 639.17 | 248.20 | 0.39 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 411.25 | 157.79 | 0.39 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 1.50 | 0.14 | 0.61 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 1.45 | 0.08 | 0.60 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 272.59 | 84.83 | 0.32 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 136.94 | 55.38 | 0.41 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.13 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | disk@materialize | 20687.90 | 16275.76 | 0.79 | 2.50 |  |
| pass | time.open | disk@materialize | 1838.65 | 1051.30 | 0.58 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 5.82 | 0.06 | 0.26 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 70.83 | 92.46 | 1.30 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 6.57 | 0.03 | 0.24 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 2.78 | 0.13 | 0.45 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 403.53 | 272.54 | 0.68 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 229.25 | 210.30 | 0.92 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 506.71 | 270.29 | 0.54 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 442.51 | 273.58 | 0.62 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 668.44 | 424.46 | 0.64 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 465.51 | 357.96 | 0.77 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 2.38 | 0.34 | 0.53 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 2.04 | 0.33 | 0.58 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 324.60 | 222.72 | 0.69 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 166.51 | 180.55 | 1.08 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.26 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | disk@stream | 20623.38 | 17226.95 | 0.84 | 2.50 |  |
| pass | time.open | disk@stream | 0.78 | 0.99 | 1.01 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 582.44 | 583.86 | 1.00 | 1.50 |  |
| pass | rss.build_peak | snapshot | 550.11 | 550.07 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 489.27 | 489.83 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 455.29 | 451.66 | 0.99 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 428.16 | 434.20 | 1.01 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 350.91 | 351.48 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 210.58 | 207.29 | 0.99 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 431.97 | 396.25 | 0.92 | 1.50 |  |
| pass | rss.after_open | disk@stream | 6.33 | 6.66 | 1.01 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 653959174.00 | 653959168.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@stream | 653959174.00 | 653959168.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 3662.91 | 12900.26 | 3.52 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 19404.31 | 73437.59 | 3.78 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 3775.71 | 6590.72 | 1.75 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 23256.61 | 32981.33 | 1.42 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 2120.43 | 5009.77 | 2.36 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 8756.91 | 13468.66 | 1.54 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2062.82 | 2529.72 | 1.23 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 8511.04 | 9861.98 | 1.16 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 185.52 | 12897.39 | 69.52 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 276.23 | 73645.66 | 266.61 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 347.63 | 6095.75 | 17.54 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 596.20 | 35041.17 | 58.77 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 162.03 | 5808.39 | 35.85 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 293.01 | 17040.36 | 58.16 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 333.94 | 2960.92 | 8.87 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 626.10 | 11274.34 | 18.01 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 156.60 | 11196.73 | 71.50 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 280.16 | 45279.14 | 161.62 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 349.50 | 4711.41 | 13.48 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 593.44 | 20486.52 | 34.52 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 164.42 | 5761.78 | 35.04 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 271.21 | 17196.26 | 63.41 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 313.63 | 2535.28 | 8.08 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 584.00 | 9926.42 | 17.00 | – | report-only |


## Concurrency against pre-Phase-6 (`pre-phase6-lubm38.json`)

| workload | backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---|---:|
| lubm-like univ 38 | snapshot | sparql | q1 | 1 | 12900.3 | 0.08 | 0.11 | 0 | identical | 22.47× |
| lubm-like univ 38 | snapshot | sparql | q1 | 8 | 73437.6 | 0.10 | 0.27 | 0 | identical | 125.44× |
| lubm-like univ 38 | snapshot | sparql | q4 | 1 | 6590.7 | 0.15 | 0.24 | 0 | identical | 384.82× |
| lubm-like univ 38 | snapshot | sparql | q4 | 8 | 32981.3 | 0.24 | 0.53 | 0 | identical | 2459.45× |
| lubm-like univ 38 | snapshot | api | q1 | 1 | 5009.8 | 0.19 | 0.40 | 0 | identical | 8.40× |
| lubm-like univ 38 | snapshot | api | q1 | 8 | 13468.7 | 0.58 | 1.34 | 0 | identical | 21.80× |
| lubm-like univ 38 | snapshot | api | q4 | 1 | 2529.7 | 0.35 | 0.67 | 0 | identical | 142.09× |
| lubm-like univ 38 | snapshot | api | q4 | 8 | 9862.0 | 0.76 | 1.71 | 0 | identical | 641.80× |
| lubm-like univ 38 | disk@materialize | sparql | q1 | 1 | 12897.4 | 0.08 | 0.11 | 0 | identical | 552.78× |
| lubm-like univ 38 | disk@materialize | sparql | q1 | 8 | 73645.7 | 0.10 | 0.27 | 0 | identical | 3276.15× |
| lubm-like univ 38 | disk@materialize | sparql | q4 | 1 | 6095.7 | 0.16 | 0.29 | 0 | identical | 483.88× |
| lubm-like univ 38 | disk@materialize | sparql | q4 | 8 | 35041.2 | 0.22 | 0.48 | 0 | identical | 2998.26× |
| lubm-like univ 38 | disk@materialize | api | q1 | 1 | 5808.4 | 0.16 | 0.35 | 0 | identical | 249.28× |
| lubm-like univ 38 | disk@materialize | api | q1 | 8 | 17040.4 | 0.46 | 1.02 | 0 | identical | 768.27× |
| lubm-like univ 38 | disk@materialize | api | q4 | 1 | 2960.9 | 0.30 | 0.57 | 0 | identical | 254.30× |
| lubm-like univ 38 | disk@materialize | api | q4 | 8 | 11274.3 | 0.66 | 1.45 | 0 | identical | 982.69× |
| lubm-like univ 38 | disk@stream | sparql | q1 | 1 | 11196.7 | 0.09 | 0.13 | 0 | identical | 479.89× |
| lubm-like univ 38 | disk@stream | sparql | q1 | 8 | 45279.1 | 0.17 | 0.42 | 0 | identical | 2014.26× |
| lubm-like univ 38 | disk@stream | sparql | q4 | 1 | 4711.4 | 0.21 | 0.38 | 0 | identical | 373.99× |
| lubm-like univ 38 | disk@stream | sparql | q4 | 8 | 20486.5 | 0.38 | 0.83 | 0 | identical | 1752.90× |
| lubm-like univ 38 | disk@stream | api | q1 | 1 | 5761.8 | 0.16 | 0.31 | 0 | identical | 247.28× |
| lubm-like univ 38 | disk@stream | api | q1 | 8 | 17196.3 | 0.45 | 1.01 | 0 | identical | 775.30× |
| lubm-like univ 38 | disk@stream | api | q4 | 1 | 2535.3 | 0.35 | 0.62 | 0 | identical | 217.75× |
| lubm-like univ 38 | disk@stream | api | q4 | 8 | 9926.4 | 0.76 | 1.71 | 0 | identical | 865.20× |


Mixed workload and `/api begin` (pre-Phase-6 has neither):

| workload | backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| lubm-like univ 38 | snapshot | sparql | off | q4 | 8 | 29837.0 | 0.26 | 0.61 | 101 / 0 | 0.94 / 1.57 | identical | – |
| lubm-like univ 38 | snapshot | sparql | on | q4 | 8 | 19.5 | 411.20 | 1363.71 | 9 / 0 | 1313.58 / 1629.48 | identical | – |
| lubm-like univ 38 | snapshot | api | off | q4 | 8 | 9341.5 | 0.78 | 1.72 | 101 / 0 | 2.18 / 2.58 | identical | – |
| lubm-like univ 38 | snapshot | api | on | q4 | 8 | 7.7 | 1042.62 | 2117.94 | 9 / 0 | 1334.50 / 2118.67 | identical | – |
| lubm-like univ 38 | disk@materialize | sparql | off | q4 | 8 | 33585.1 | 0.23 | 0.47 | 101 / 0 | 5.81 / 5.23 | identical | – |
| lubm-like univ 38 | disk@materialize | sparql | on | q4 | 8 | 33723.0 | 0.23 | 0.48 | 101 / 0 | 4.11 / 6.39 | identical | – |
| lubm-like univ 38 | disk@materialize | api | off | q4 | 8 | 11411.1 | 0.64 | 1.42 | 101 / 0 | 4.30 / 6.19 | identical | – |
| lubm-like univ 38 | disk@materialize | api | on | q4 | 8 | 10925.6 | 0.67 | 1.52 | 101 / 0 | 4.37 / 5.97 | identical | – |
| lubm-like univ 38 | disk@stream | sparql | off | q4 | 8 | 20838.3 | 0.38 | 0.82 | 101 / 0 | 3.94 / 5.45 | identical | – |
| lubm-like univ 38 | disk@stream | sparql | on | q4 | 8 | 21122.9 | 0.37 | 0.76 | 101 / 0 | 6.22 / 5.91 | identical | – |
| lubm-like univ 38 | disk@stream | api | off | q4 | 8 | 9676.5 | 0.76 | 1.73 | 101 / 0 | 5.81 / 6.13 | identical | – |
| lubm-like univ 38 | disk@stream | api | on | q4 | 8 | 9407.4 | 0.78 | 1.78 | 101 / 0 | 4.05 / 5.41 | identical | – |

| workload | backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---|---:|---:|---:|---:|---:|---:|
| lubm-like univ 38 | snapshot | 20 | 0.32 | 0.54 | 0.69 | 0.0 / 0.0 | – |
| lubm-like univ 38 | disk@materialize | 20 | 0.28 | 0.51 | 0.73 | 0.0 / 0.0 | – |
| lubm-like univ 38 | disk@stream | 20 | 0.37 | 0.54 | 0.90 | 0.1 / 0.1 | – |

