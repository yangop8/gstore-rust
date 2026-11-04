## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.06 0.34 0.62
- calibration score 70.47 ms (sort 43.81, hashmap 191.69, format 41.68, fsync 0.50)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk@stream | 3.65 | 96.9 | 130.2 | 4.1 (cold) | 0.8 | 6.0 | 66.0 | 3.06 | 0.98× | 1.27× | 0.46× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@stream | parse+encode 3209, spo 122, index 199, pos 88, osp+vstree 198, dictionary 79, finish 106, install 2 | 1024.0 | 80.0 | 24.1 | 4 | 0 | 0.0 |

| query | rows | disk@stream first ms | disk@stream median ms | disk@stream vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.31 | 0.02 | 0.04× |
| q2 | 25 | 14.94 | 6.98 | 0.64× |
| q3 | 10 | 0.11 | 0.02 | 0.04× |
| q4 | 9 | 0.15 | 0.07 | 0.35× |
| q5 | 576 | 34.97 | 28.93 | 0.83× |
| q6 | 80056 | 50.43 | 32.47 | 0.92× |
| q7 | 23 | 38.51 | 29.31 | 0.66× |
| q8 | 10778 | 50.95 | 45.77 | 0.77× |
| q9 | 1081 | 81.28 | 65.06 | 0.74× |
| q10 | 3 | 30.29 | 28.81 | 0.93× |
| q11 | 325 | 0.78 | 0.21 | 0.48× |
| q12 | 21 | 0.37 | 0.09 | 0.54× |
| q13 | 85670 | 40.82 | 35.95 | 0.76× |
| q14 | 60576 | 26.13 | 25.10 | 1.01× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk@stream | 2.01 / 3.45 | 2.71 / 4.32 | 5330 | 562.4 | 1000.7 | 0.1 | 0.8 | 168432 → 168432 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk@stream | sparql | q1 | 1 | 10774.6 | 0.09 | 0.15 | 0 | identical | 6.01× |
| disk@stream | sparql | q1 | 8 | 45735.6 | 0.17 | 0.40 | 0 | identical | 9.89× |
| disk@stream | sparql | q4 | 1 | 4660.2 | 0.21 | 0.37 | 0 | identical | 2.03× |
| disk@stream | sparql | q4 | 8 | 20254.9 | 0.39 | 0.84 | 0 | identical | 2.63× |
| disk@stream | api | q1 | 1 | 4604.4 | 0.21 | 0.40 | 0 | identical | 3.23× |
| disk@stream | api | q1 | 8 | 15193.7 | 0.51 | 1.16 | 0 | identical | 3.61× |
| disk@stream | api | q4 | 1 | 2017.7 | 0.45 | 0.74 | 0 | identical | 1.33× |
| disk@stream | api | q4 | 8 | 8832.3 | 0.85 | 1.86 | 0 | identical | 2.08× |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| disk@stream | sparql | off | q4 | 8 | 20557.6 | 0.38 | 0.85 | 51 / 0 | 3.90 / 5.64 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 20972.9 | 0.37 | 0.82 | 51 / 0 | 3.62 / 5.38 | identical | – |
| disk@stream | api | off | q4 | 8 | 7644.0 | 0.97 | 2.04 | 51 / 0 | 11.15 / 22.94 | identical | – |
| disk@stream | api | on | q4 | 8 | 8064.2 | 0.91 | 2.03 | 51 / 0 | 4.63 / 6.64 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| disk@stream | 20 | 0.40 | 0.83 | 1.02 | 0.1 / 0.1 | – |

### Gates: PASS (106 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk@stream/q1 | 0.42 | 0.02 | 0.83 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 10.97 | 6.29 | 0.64 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 0.47 | 0.02 | 0.82 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 0.21 | 0.07 | 0.93 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 34.97 | 26.08 | 0.76 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 35.30 | 29.27 | 0.84 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 44.54 | 26.42 | 0.61 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 59.15 | 41.26 | 0.71 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 88.38 | 58.65 | 0.67 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 30.84 | 25.97 | 0.85 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 0.43 | 0.19 | 0.90 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.17 | 0.08 | 0.96 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 47.42 | 32.41 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 24.76 | 22.63 | 0.92 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.72 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk@stream | 3732.19 | 3286.90 | 0.88 | 2.50 |  |
| pass | time.open | disk@stream | 0.64 | 0.73 | 1.00 | 2.50 |  |
| pass | rss.query_hwm | disk@stream | 69.21 | 65.95 | 0.97 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 108.86 | 96.93 | 0.92 | 1.50 |  |
| pass | rss.after_open | disk@stream | 5.93 | 6.05 | 1.00 | 1.50 |  |
| pass | bytes.dir | disk@stream | 130150405.00 | 130150400.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 1792.72 | 10774.60 | 6.01 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 4625.19 | 45735.64 | 9.89 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 2293.81 | 4660.19 | 2.03 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 7694.01 | 20254.93 | 2.63 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 1425.11 | 4604.40 | 3.23 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 4212.39 | 15193.69 | 3.61 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 1520.17 | 2017.69 | 1.33 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 4247.16 | 8832.27 | 2.08 | – | report-only |

