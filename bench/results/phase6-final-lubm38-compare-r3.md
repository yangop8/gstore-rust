## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 4.46 3.27 1.86
- calibration score 75.83 ms (sort 47.29, hashmap 224.35, format 41.10, fsync 0.89)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 29.60 | 550.1 | 430.8 | 1184.8 (cold) | 1033.0 | 489.1 | 601.0 | 10.15 | 1.03× | 1.02× | 0.63× |
| disk@materialize | 21.06 | 400.3 | 654.0 | 1817.3 (cold) | 1720.8 | 351.0 | 457.1 | 7.83 | 1.00× | 1.22× | 0.20× |
| disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.87× | 0.00× | 0.29× |
| rocksdb | 19.50 | 401.6 | 155.7 | 738.9 (cold) | 678.0 | 270.8 | 436.2 | 25.00 | 0.88× | 0.75× | 0.37× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 18861, spo 721, index 922, pos 472, osp+vstree 921, dictionary 453, finish 531, install 4 | 1024.0 | 384.2 | 155.0 | 4 | 0 | 82.7 |
| disk@stream | parse+encode 16110, spo 736, index 1044, pos 536, osp+vstree 1044, dictionary 378, finish 477, install 3 | 1024.0 | 352.2 | 155.0 | 4 | 0 | 82.7 |
| rocksdb | parse+encode 16799, spo 1490, index 1080, pos 700, osp 740, x-counts 608, dictionary 203, ingest 94, install 1 | 1024.0 | 314.7 | 155.0 | 4 | 0 | 0.0 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.19 | 0.01 | 0.12× | 0.17 | 0.01 | 0.00× | 0.31 | 0.02 | 0.00× | 0.35 | 0.04 | 0.00× |
| q2 | 115 | 39.03 | 37.56 | 0.99× | 32.84 | 28.64 | 0.56× | 85.32 | 56.64 | 1.11× | 580.79 | 585.74 | 0.99× |
| q3 | 10 | 0.09 | 0.01 | 0.04× | 0.09 | 0.01 | 0.00× | 0.17 | 0.02 | 0.00× | 0.32 | 0.07 | 0.00× |
| q4 | 9 | 0.10 | 0.05 | 0.51× | 0.10 | 0.05 | 0.02× | 0.20 | 0.07 | 0.03× | 0.36 | 0.26 | 0.17× |
| q5 | 576 | 265.79 | 234.72 | 1.18× | 264.95 | 232.52 | 0.88× | 328.85 | 274.10 | 1.04× | 395.86 | 353.87 | 1.00× |
| q6 | 404399 | 82.44 | 80.77 | 0.66× | 92.64 | 91.93 | 0.56× | 265.73 | 194.76 | 1.19× | 224.42 | 232.11 | 0.97× |
| q7 | 23 | 277.73 | 256.74 | 0.98× | 251.28 | 256.17 | 0.71× | 368.96 | 278.73 | 0.78× | 435.28 | 394.93 | 1.04× |
| q8 | 10778 | 238.14 | 244.63 | 1.12× | 215.27 | 274.91 | 0.74× | 253.72 | 256.85 | 0.69× | 384.88 | 421.28 | 1.04× |
| q9 | 5507 | 333.54 | 343.83 | 1.12× | 329.93 | 347.49 | 0.71× | 515.48 | 521.65 | 1.06× | 721.98 | 742.08 | 1.00× |
| q10 | 3 | 220.69 | 231.74 | 1.19× | 224.12 | 232.61 | 0.83× | 236.31 | 219.28 | 0.79× | 369.32 | 400.01 | 1.05× |
| q11 | 325 | 2.70 | 1.31 | 1.21× | 0.50 | 0.17 | 0.18× | 1.20 | 0.26 | 0.27× | 11.57 | 2.01 | 0.93× |
| q12 | 21 | 1.05 | 0.49 | 1.02× | 0.17 | 0.08 | 0.15× | 1.15 | 0.17 | 0.30× | 0.44 | 0.22 | 0.71× |
| q13 | 432512 | 100.11 | 96.02 | 0.69× | 105.42 | 107.74 | 0.45× | 217.72 | 215.91 | 0.91× | 247.34 | 257.25 | 0.97× |
| q14 | 305503 | 62.49 | 62.22 | 0.71× | 71.71 | 72.48 | 0.61× | 151.63 | 155.96 | 1.31× | 187.78 | 208.59 | 1.00× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.01 / 0.17 | 0.01 / 0.01 | 6478 | 41.7 | 31.6 | 1156.6 | 780.5 | 842657 → 842657 | ok |
| disk@materialize | 1.70 / 3.16 | 1.85 / 2.62 | 6478 | 949.5 | 954.7 | 0.1 | 2797.7 | 842657 → 842657 | ok |
| disk@stream | 1.98 / 2.83 | 2.57 / 3.62 | 6478 | 938.3 | 1015.3 | 0.1 | 1.0 | 842657 → 842657 | ok |
| rocksdb | 0.20 / 0.37 | 0.25 / 0.46 | 6478 | 131.9 | 128.8 | 43.3 | 641.5 | 842657 → 842657 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 11539.8 | 0.09 | 0.14 | 0 | identical | 2.25× |
| snapshot | sparql | q1 | 8 | 74691.0 | 0.10 | 0.28 | 0 | identical | 2.97× |
| snapshot | sparql | q4 | 1 | 6223.2 | 0.16 | 0.25 | 0 | identical | 1.41× |
| snapshot | sparql | q4 | 8 | 33157.8 | 0.24 | 0.52 | 0 | identical | 1.42× |
| snapshot | api | q1 | 1 | 5391.5 | 0.17 | 0.35 | 0 | identical | 1.81× |
| snapshot | api | q1 | 8 | 15417.4 | 0.50 | 1.18 | 0 | identical | 1.48× |
| snapshot | api | q4 | 1 | 2504.1 | 0.36 | 0.66 | 0 | identical | 1.06× |
| snapshot | api | q4 | 8 | 11600.3 | 0.64 | 1.42 | 0 | identical | 1.10× |
| disk@materialize | sparql | q1 | 1 | 12086.2 | 0.08 | 0.12 | 0 | identical | 61.78× |
| disk@materialize | sparql | q1 | 8 | 73472.2 | 0.10 | 0.28 | 0 | identical | 165.39× |
| disk@materialize | sparql | q4 | 1 | 5795.0 | 0.17 | 0.30 | 0 | identical | 15.19× |
| disk@materialize | sparql | q4 | 8 | 32814.9 | 0.24 | 0.52 | 0 | identical | 35.64× |
| disk@materialize | api | q1 | 1 | 5379.9 | 0.18 | 0.34 | 0 | identical | 26.80× |
| disk@materialize | api | q1 | 8 | 15604.2 | 0.50 | 1.16 | 0 | identical | 37.45× |
| disk@materialize | api | q4 | 1 | 2596.5 | 0.34 | 0.61 | 0 | identical | 7.30× |
| disk@materialize | api | q4 | 8 | 11694.5 | 0.64 | 1.39 | 0 | identical | 12.88× |
| disk@stream | sparql | q1 | 1 | 10898.0 | 0.09 | 0.13 | 0 | identical | 55.70× |
| disk@stream | sparql | q1 | 8 | 42034.8 | 0.19 | 0.44 | 0 | identical | 94.62× |
| disk@stream | sparql | q4 | 1 | 4801.1 | 0.21 | 0.37 | 0 | identical | 12.59× |
| disk@stream | sparql | q4 | 8 | 19492.9 | 0.40 | 0.86 | 0 | identical | 21.17× |
| disk@stream | api | q1 | 1 | 4698.6 | 0.20 | 0.38 | 0 | identical | 23.41× |
| disk@stream | api | q1 | 8 | 13915.2 | 0.56 | 1.27 | 0 | identical | 33.39× |
| disk@stream | api | q4 | 1 | 1967.6 | 0.46 | 0.73 | 0 | identical | 5.53× |
| disk@stream | api | q4 | 8 | 9309.4 | 0.81 | 1.72 | 0 | identical | 10.25× |
| rocksdb | sparql | q1 | 1 | 8236.1 | 0.12 | 0.20 | 0 | identical | – |
| rocksdb | sparql | q1 | 8 | 49920.8 | 0.15 | 0.40 | 0 | identical | – |
| rocksdb | sparql | q4 | 1 | 2529.2 | 0.39 | 0.64 | 0 | identical | – |
| rocksdb | sparql | q4 | 8 | 10860.0 | 0.73 | 1.60 | 0 | identical | – |
| rocksdb | api | q1 | 1 | 3781.1 | 0.25 | 0.49 | 0 | identical | – |
| rocksdb | api | q1 | 8 | 11894.6 | 0.65 | 1.50 | 0 | identical | – |
| rocksdb | api | q4 | 1 | 1629.8 | 0.57 | 0.91 | 0 | identical | – |
| rocksdb | api | q4 | 8 | 6206.7 | 1.23 | 2.62 | 0 | identical | – |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | off | q4 | 8 | 31966.8 | 0.24 | 0.54 | 51 / 0 | 0.81 / 1.53 | identical | – |
| snapshot | sparql | on | q4 | 8 | 12.5 | 639.14 | 1124.47 | 6 / 0 | 1089.12 / 1126.97 | identical | – |
| snapshot | api | off | q4 | 8 | 10764.0 | 0.68 | 1.48 | 51 / 0 | 1.44 / 3.14 | identical | – |
| snapshot | api | on | q4 | 8 | 10.1 | 791.24 | 1111.74 | 6 / 0 | 1065.65 / 1111.98 | identical | – |
| disk@materialize | sparql | off | q4 | 8 | 31264.8 | 0.25 | 0.54 | 51 / 0 | 6.00 / 4.97 | identical | – |
| disk@materialize | sparql | on | q4 | 8 | 30304.8 | 0.25 | 0.56 | 51 / 0 | 4.04 / 6.45 | identical | – |
| disk@materialize | api | off | q4 | 8 | 10306.0 | 0.71 | 1.61 | 51 / 0 | 4.40 / 5.52 | identical | – |
| disk@materialize | api | on | q4 | 8 | 9101.0 | 0.80 | 1.82 | 51 / 0 | 4.58 / 6.28 | identical | – |
| disk@stream | sparql | off | q4 | 8 | 20629.9 | 0.38 | 0.82 | 51 / 0 | 3.60 / 4.75 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 19226.9 | 0.41 | 0.85 | 51 / 0 | 8.04 / 5.87 | identical | – |
| disk@stream | api | off | q4 | 8 | 8437.2 | 0.87 | 1.90 | 51 / 0 | 4.58 / 6.13 | identical | – |
| disk@stream | api | on | q4 | 8 | 8675.0 | 0.85 | 1.93 | 51 / 0 | 4.37 / 6.14 | identical | – |
| rocksdb | sparql | off | q4 | 8 | 8552.1 | 0.93 | 1.94 | 51 / 0 | 2.28 / 3.36 | identical | – |
| rocksdb | sparql | on | q4 | 8 | 894.3 | 8.93 | 4.54 | 19 / 0 | 284.88 / 4943.28 | identical | – |
| rocksdb | api | off | q4 | 8 | 5692.0 | 1.32 | 2.74 | 51 / 0 | 3.12 / 4.34 | identical | – |
| rocksdb | api | on | q4 | 8 | 545.1 | 14.58 | 7.51 | 22 / 0 | 262.03 / 1678.29 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 20 | 0.38 | 0.51 | 1.13 | 0.1 / 0.1 | – |
| disk@materialize | 20 | 0.38 | 0.60 | 0.93 | 0.0 / 0.0 | – |
| disk@stream | 20 | 0.44 | 0.59 | 0.62 | 0.1 / 0.1 | – |
| rocksdb | 20 | 2132.55 | 3013.19 | 3191.01 | 269.3 / 269.3 | – |

### Gates: PASS (365 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.10 | 0.01 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q2 | 38.03 | 30.04 | 0.80 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.40 | 0.01 | 0.84 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.10 | 0.04 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q5 | 199.00 | 187.72 | 0.94 | 3.00 |  |
| pass | time.query | snapshot/q6 | 122.03 | 64.60 | 0.54 | 3.00 |  |
| pass | time.query | snapshot/q7 | 263.02 | 205.34 | 0.78 | 3.00 |  |
| pass | time.query | snapshot/q8 | 217.78 | 195.65 | 0.90 | 3.00 |  |
| pass | time.query | snapshot/q9 | 307.39 | 274.99 | 0.90 | 3.00 |  |
| pass | time.query | snapshot/q10 | 194.07 | 185.34 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.08 | 1.04 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.48 | 0.39 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q13 | 140.12 | 76.79 | 0.55 | 3.00 |  |
| pass | time.query | snapshot/q14 | 87.90 | 49.76 | 0.58 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.77 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 28843.16 | 23676.26 | 0.82 | 2.00 |  |
| pass | time.open | snapshot | 1015.15 | 826.21 | 0.82 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 4.22 | 0.01 | 0.32 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 51.03 | 22.39 | 0.46 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 4.63 | 0.01 | 0.30 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 2.12 | 0.04 | 0.49 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 264.42 | 181.77 | 0.69 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 163.68 | 71.87 | 0.45 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 358.48 | 200.25 | 0.56 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 371.78 | 214.90 | 0.58 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 490.84 | 271.64 | 0.56 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 278.67 | 181.83 | 0.65 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 0.96 | 0.14 | 0.72 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 0.55 | 0.06 | 0.81 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 238.14 | 84.23 | 0.36 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 118.82 | 56.66 | 0.49 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.16 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | disk@materialize | 21012.10 | 16465.76 | 0.78 | 2.50 |  |
| pass | time.open | disk@materialize | 1407.22 | 1345.17 | 0.96 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 4.22 | 0.02 | 0.32 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 51.03 | 49.12 | 0.96 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 4.63 | 0.01 | 0.30 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 2.12 | 0.06 | 0.50 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 264.42 | 237.72 | 0.90 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 163.68 | 168.91 | 1.03 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 358.48 | 241.74 | 0.68 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 371.78 | 222.76 | 0.60 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 490.84 | 452.41 | 0.92 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 278.67 | 190.17 | 0.68 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 0.96 | 0.22 | 0.75 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.55 | 0.14 | 0.84 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 238.14 | 187.25 | 0.79 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 118.82 | 135.26 | 1.14 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.25 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | disk@stream | 21012.10 | 15943.71 | 0.76 | 2.50 |  |
| pass | time.open | disk@stream | 1407.22 | 0.79 | 0.01 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.43 | 0.03 | 0.18 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 589.62 | 556.89 | 0.94 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 30.74 | 0.07 | 0.06 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 1.53 | 0.25 | 0.64 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 355.15 | 336.44 | 0.95 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 239.14 | 220.68 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 379.34 | 375.48 | 0.99 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 403.36 | 400.53 | 0.99 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 740.81 | 705.54 | 0.95 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 381.82 | 380.31 | 1.00 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.15 | 1.91 | 0.94 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 0.31 | 0.21 | 0.96 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 266.48 | 244.58 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 208.40 | 198.32 | 0.95 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.34 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | rocksdb | 22248.31 | 18535.57 | 0.83 | 2.00 |  |
| pass | time.open | rocksdb | 900.72 | 644.61 | 0.72 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 588.53 | 600.99 | 1.02 | 1.50 |  |
| pass | rss.build_peak | snapshot | 550.00 | 550.09 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 488.97 | 489.11 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 460.73 | 457.07 | 0.99 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 434.44 | 400.31 | 0.93 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 350.69 | 350.98 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 460.73 | 207.27 | 0.49 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 434.44 | 398.18 | 0.92 | 1.50 |  |
| pass | rss.after_open | disk@stream | 350.69 | 6.13 | 0.10 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 440.21 | 436.19 | 0.99 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 401.55 | 401.63 | 1.00 | 1.50 |  |
| pass | rss.after_open | rocksdb | 276.59 | 270.78 | 0.98 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 653959173.00 | 653959168.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@stream | 653959173.00 | 653959168.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 155868540.00 | 155661891.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 5139.55 | 11539.82 | 2.25 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 25106.37 | 74691.02 | 2.97 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 4404.40 | 6223.20 | 1.41 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 23334.23 | 33157.81 | 1.42 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 2972.53 | 5391.49 | 1.81 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 10444.90 | 15417.40 | 1.48 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2373.40 | 2504.07 | 1.06 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 10506.16 | 11600.33 | 1.10 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 195.64 | 12086.25 | 61.78 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 444.25 | 73472.17 | 165.39 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 381.48 | 5794.96 | 15.19 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 920.72 | 32814.89 | 35.64 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 200.75 | 5379.95 | 26.80 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 416.70 | 15604.18 | 37.45 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 355.51 | 2596.51 | 7.30 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 908.26 | 11694.46 | 12.88 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 195.64 | 10898.00 | 55.70 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 444.25 | 42034.77 | 94.62 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 381.48 | 4801.15 | 12.59 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 920.72 | 19492.94 | 21.17 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 200.75 | 4698.58 | 23.41 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 416.70 | 13915.18 | 33.39 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 355.51 | 1967.57 | 5.53 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 908.26 | 9309.44 | 10.25 | – | report-only |

