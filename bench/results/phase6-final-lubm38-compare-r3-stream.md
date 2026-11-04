## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 4.46 3.27 1.86
- calibration score 75.83 ms (sort 47.29, hashmap 224.35, format 41.10, fsync 0.89)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.92× | 1.47× | 0.26× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@stream | parse+encode 16110, spo 736, index 1044, pos 536, osp+vstree 1044, dictionary 378, finish 477, install 3 | 1024.0 | 352.2 | 155.0 | 4 | 0 | 82.7 |

| query | rows | disk@stream first ms | disk@stream median ms | disk@stream vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.31 | 0.02 | 0.00× |
| q2 | 115 | 85.32 | 56.64 | 0.78× |
| q3 | 10 | 0.17 | 0.02 | 0.00× |
| q4 | 9 | 0.20 | 0.07 | 0.03× |
| q5 | 576 | 328.85 | 274.10 | 1.13× |
| q6 | 404399 | 265.73 | 194.76 | 0.90× |
| q7 | 23 | 368.96 | 278.73 | 0.80× |
| q8 | 10778 | 253.72 | 256.85 | 0.83× |
| q9 | 5507 | 515.48 | 521.65 | 0.88× |
| q10 | 3 | 236.31 | 219.28 | 0.83× |
| q11 | 325 | 1.20 | 0.26 | 0.19× |
| q12 | 21 | 1.15 | 0.17 | 0.25× |
| q13 | 432512 | 217.72 | 215.91 | 0.76× |
| q14 | 305503 | 151.63 | 155.96 | 1.01× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk@stream | 1.98 / 2.83 | 2.57 / 3.62 | 6478 | 938.3 | 1015.3 | 0.1 | 1.0 | 842657 → 842657 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk@stream | sparql | q1 | 1 | 10898.0 | 0.09 | 0.13 | 0 | identical | 55.98× |
| disk@stream | sparql | q1 | 8 | 42034.8 | 0.19 | 0.44 | 0 | identical | 97.72× |
| disk@stream | sparql | q4 | 1 | 4801.1 | 0.21 | 0.37 | 0 | identical | 12.64× |
| disk@stream | sparql | q4 | 8 | 19492.9 | 0.40 | 0.86 | 0 | identical | 20.44× |
| disk@stream | api | q1 | 1 | 4698.6 | 0.20 | 0.38 | 0 | identical | 25.37× |
| disk@stream | api | q1 | 8 | 13915.2 | 0.56 | 1.27 | 0 | identical | 31.35× |
| disk@stream | api | q4 | 1 | 1967.6 | 0.46 | 0.73 | 0 | identical | 5.54× |
| disk@stream | api | q4 | 8 | 9309.4 | 0.81 | 1.72 | 0 | identical | 9.96× |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| disk@stream | sparql | off | q4 | 8 | 20629.9 | 0.38 | 0.82 | 51 / 0 | 3.60 / 4.75 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 19226.9 | 0.41 | 0.85 | 51 / 0 | 8.04 / 5.87 | identical | – |
| disk@stream | api | off | q4 | 8 | 8437.2 | 0.87 | 1.90 | 51 / 0 | 4.58 / 6.13 | identical | – |
| disk@stream | api | on | q4 | 8 | 8675.0 | 0.85 | 1.93 | 51 / 0 | 4.37 / 6.14 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| disk@stream | 20 | 0.44 | 0.59 | 0.62 | 0.1 / 0.1 | – |

### Gates: PASS (106 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk@stream/q1 | 4.14 | 0.02 | 0.33 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 72.75 | 47.38 | 0.66 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 4.89 | 0.01 | 0.29 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 2.15 | 0.06 | 0.50 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 242.66 | 229.32 | 0.95 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 215.59 | 162.94 | 0.76 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 346.40 | 233.20 | 0.68 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 308.28 | 214.89 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 591.06 | 436.43 | 0.74 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 263.86 | 183.46 | 0.70 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 1.33 | 0.21 | 0.66 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.66 | 0.14 | 0.80 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 283.39 | 180.63 | 0.64 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 153.70 | 130.48 | 0.85 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.22 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk@stream | 20064.31 | 15380.57 | 0.77 | 2.50 |  |
| pass | time.open | disk@stream | 0.62 | 0.77 | 1.01 | 2.50 |  |
| pass | rss.query_hwm | disk@stream | 210.85 | 207.27 | 0.99 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 428.72 | 398.18 | 0.93 | 1.50 |  |
| pass | rss.after_open | disk@stream | 5.82 | 6.13 | 1.01 | 1.50 |  |
| pass | bytes.dir | disk@stream | 653959173.00 | 653959168.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 194.67 | 10898.00 | 55.98 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 430.16 | 42034.77 | 97.72 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 379.83 | 4801.15 | 12.64 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 953.54 | 19492.94 | 20.44 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 185.24 | 4698.58 | 25.37 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 443.83 | 13915.18 | 31.35 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 355.25 | 1967.57 | 5.54 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 934.31 | 9309.44 | 9.96 | – | report-only |

