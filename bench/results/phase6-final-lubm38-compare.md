## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 4.46 3.27 1.86
- calibration score 75.83 ms (sort 47.29, hashmap 224.35, format 41.10, fsync 0.89)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 29.60 | 550.1 | 430.8 | 1184.8 (cold) | 1033.0 | 489.1 | 601.0 | 10.15 | 0.39× | 0.71× | 0.22× |
| disk@materialize | 21.06 | 400.3 | 654.0 | 1817.3 (cold) | 1720.8 | 351.0 | 457.1 | 7.83 | 0.01× | 0.22× | 0.05× |
| disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.01× | 0.00× | 0.08× |
| rocksdb | 19.50 | 401.6 | 155.7 | 738.9 (cold) | 678.0 | 270.8 | 436.2 | 25.00 | 0.07× | 0.01× | 0.08× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 18861, spo 721, index 922, pos 472, osp+vstree 921, dictionary 453, finish 531, install 4 | 1024.0 | 384.2 | 155.0 | 4 | 0 | 82.7 |
| disk@stream | parse+encode 16110, spo 736, index 1044, pos 536, osp+vstree 1044, dictionary 378, finish 477, install 3 | 1024.0 | 352.2 | 155.0 | 4 | 0 | 82.7 |
| rocksdb | parse+encode 16799, spo 1490, index 1080, pos 700, osp 740, x-counts 608, dictionary 203, ingest 94, install 1 | 1024.0 | 314.7 | 155.0 | 4 | 0 | 0.0 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.19 | 0.01 | 0.01× | 0.17 | 0.01 | 0.00× | 0.31 | 0.02 | 0.00× | 0.35 | 0.04 | 0.00× |
| q2 | 115 | 39.03 | 37.56 | 0.74× | 32.84 | 28.64 | 0.15× | 85.32 | 56.64 | 0.30× | 580.79 | 585.74 | 0.80× |
| q3 | 10 | 0.09 | 0.01 | 0.01× | 0.09 | 0.01 | 0.00× | 0.17 | 0.02 | 0.00× | 0.32 | 0.07 | 0.00× |
| q4 | 9 | 0.10 | 0.05 | 0.00× | 0.10 | 0.05 | 0.00× | 0.20 | 0.07 | 0.00× | 0.36 | 0.26 | 0.00× |
| q5 | 576 | 265.79 | 234.72 | 0.70× | 264.95 | 232.52 | 0.37× | 328.85 | 274.10 | 0.44× | 395.86 | 353.87 | 0.66× |
| q6 | 404399 | 82.44 | 80.77 | 0.43× | 92.64 | 91.93 | 0.18× | 265.73 | 194.76 | 0.38× | 224.42 | 232.11 | 0.50× |
| q7 | 23 | 277.73 | 256.74 | 0.64× | 251.28 | 256.17 | 0.22× | 368.96 | 278.73 | 0.24× | 435.28 | 394.93 | 0.40× |
| q8 | 10778 | 238.14 | 244.63 | 0.67× | 215.27 | 274.91 | 0.32× | 253.72 | 256.85 | 0.29× | 384.88 | 421.28 | 0.47× |
| q9 | 5507 | 333.54 | 343.83 | 0.65× | 329.93 | 347.49 | 0.24× | 515.48 | 521.65 | 0.36× | 721.98 | 742.08 | 0.35× |
| q10 | 3 | 220.69 | 231.74 | 0.71× | 224.12 | 232.61 | 0.33× | 236.31 | 219.28 | 0.31× | 369.32 | 400.01 | 0.67× |
| q11 | 325 | 2.70 | 1.31 | 1.20× | 0.50 | 0.17 | 0.12× | 1.20 | 0.26 | 0.17× | 11.57 | 2.01 | 0.12× |
| q12 | 21 | 1.05 | 0.49 | 0.84× | 0.17 | 0.08 | 0.11× | 1.15 | 0.17 | 0.23× | 0.44 | 0.22 | 0.02× |
| q13 | 432512 | 100.11 | 96.02 | 0.43× | 105.42 | 107.74 | 0.12× | 217.72 | 215.91 | 0.24× | 247.34 | 257.25 | 0.50× |
| q14 | 305503 | 62.49 | 62.22 | 0.47× | 71.71 | 72.48 | 0.23× | 151.63 | 155.96 | 0.49× | 187.78 | 208.59 | 0.55× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.01 / 0.17 | 0.01 / 0.01 | 6478 | 41.7 | 31.6 | 1156.6 | 780.5 | 842657 → 842657 | ok |
| disk@materialize | 1.70 / 3.16 | 1.85 / 2.62 | 6478 | 949.5 | 954.7 | 0.1 | 2797.7 | 842657 → 842657 | ok |
| disk@stream | 1.98 / 2.83 | 2.57 / 3.62 | 6478 | 938.3 | 1015.3 | 0.1 | 1.0 | 842657 → 842657 | ok |
| rocksdb | 0.20 / 0.37 | 0.25 / 0.46 | 6478 | 131.9 | 128.8 | 43.3 | 641.5 | 842657 → 842657 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 11539.8 | 0.09 | 0.14 | 0 | identical | 20.10× |
| snapshot | sparql | q1 | 8 | 74691.0 | 0.10 | 0.28 | 0 | identical | 127.58× |
| snapshot | sparql | q4 | 1 | 6223.2 | 0.16 | 0.25 | 0 | identical | 363.36× |
| snapshot | sparql | q4 | 8 | 33157.8 | 0.24 | 0.52 | 0 | identical | 2472.61× |
| snapshot | api | q1 | 1 | 5391.5 | 0.17 | 0.35 | 0 | identical | 9.04× |
| snapshot | api | q1 | 8 | 15417.4 | 0.50 | 1.18 | 0 | identical | 24.95× |
| snapshot | api | q4 | 1 | 2504.1 | 0.36 | 0.66 | 0 | identical | 140.65× |
| snapshot | api | q4 | 8 | 11600.3 | 0.64 | 1.42 | 0 | identical | 754.93× |
| disk@materialize | sparql | q1 | 1 | 12086.2 | 0.08 | 0.12 | 0 | identical | 518.02× |
| disk@materialize | sparql | q1 | 8 | 73472.2 | 0.10 | 0.28 | 0 | identical | 3268.44× |
| disk@materialize | sparql | q4 | 1 | 5795.0 | 0.17 | 0.30 | 0 | identical | 460.01× |
| disk@materialize | sparql | q4 | 8 | 32814.9 | 0.24 | 0.52 | 0 | identical | 2807.77× |
| disk@materialize | api | q1 | 1 | 5379.9 | 0.18 | 0.34 | 0 | identical | 230.89× |
| disk@materialize | api | q1 | 8 | 15604.2 | 0.50 | 1.16 | 0 | identical | 703.52× |
| disk@materialize | api | q4 | 1 | 2596.5 | 0.34 | 0.61 | 0 | identical | 223.01× |
| disk@materialize | api | q4 | 8 | 11694.5 | 0.64 | 1.39 | 0 | identical | 1019.30× |
| disk@stream | sparql | q1 | 1 | 10898.0 | 0.09 | 0.13 | 0 | identical | 467.09× |
| disk@stream | sparql | q1 | 8 | 42034.8 | 0.19 | 0.44 | 0 | identical | 1869.93× |
| disk@stream | sparql | q4 | 1 | 4801.1 | 0.21 | 0.37 | 0 | identical | 381.12× |
| disk@stream | sparql | q4 | 8 | 19492.9 | 0.40 | 0.86 | 0 | identical | 1667.89× |
| disk@stream | api | q1 | 1 | 4698.6 | 0.20 | 0.38 | 0 | identical | 201.65× |
| disk@stream | api | q1 | 8 | 13915.2 | 0.56 | 1.27 | 0 | identical | 627.37× |
| disk@stream | api | q4 | 1 | 1967.6 | 0.46 | 0.73 | 0 | identical | 168.99× |
| disk@stream | api | q4 | 8 | 9309.4 | 0.81 | 1.72 | 0 | identical | 811.42× |
| rocksdb | sparql | q1 | 1 | 8236.1 | 0.12 | 0.20 | 0 | identical | 546.66× |
| rocksdb | sparql | q1 | 8 | 49920.8 | 0.15 | 0.40 | 0 | identical | 3476.00× |
| rocksdb | sparql | q4 | 1 | 2529.2 | 0.39 | 0.64 | 0 | identical | 2114.26× |
| rocksdb | sparql | q4 | 8 | 10860.0 | 0.73 | 1.60 | 0 | identical | 9340.00× |
| rocksdb | api | q1 | 1 | 3781.1 | 0.25 | 0.49 | 0 | identical | 238.77× |
| rocksdb | api | q1 | 8 | 11894.6 | 0.65 | 1.50 | 0 | identical | 787.67× |
| rocksdb | api | q4 | 1 | 1629.8 | 0.57 | 0.91 | 0 | identical | 1347.27× |
| rocksdb | api | q4 | 8 | 6206.7 | 1.23 | 2.62 | 0 | identical | 5329.54× |

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

### Gates: PASS (373 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 1.30 | 0.01 | 0.61 | 3.00 |  |
| pass | time.query | snapshot/q2 | 50.80 | 31.00 | 0.62 | 3.00 |  |
| pass | time.query | snapshot/q3 | 2.19 | 0.01 | 0.48 | 3.00 |  |
| pass | time.query | snapshot/q4 | 53.63 | 0.04 | 0.04 | 3.00 |  |
| pass | time.query | snapshot/q5 | 333.25 | 193.70 | 0.58 | 3.00 |  |
| pass | time.query | snapshot/q6 | 187.61 | 66.65 | 0.36 | 3.00 |  |
| pass | time.query | snapshot/q7 | 402.96 | 211.88 | 0.53 | 3.00 |  |
| pass | time.query | snapshot/q8 | 363.48 | 201.88 | 0.56 | 3.00 |  |
| pass | time.query | snapshot/q9 | 529.28 | 283.75 | 0.54 | 3.00 |  |
| pass | time.query | snapshot/q10 | 326.63 | 191.25 | 0.59 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.09 | 1.08 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.59 | 0.41 | 0.93 | 3.00 |  |
| pass | time.query | snapshot/q13 | 223.92 | 79.24 | 0.36 | 3.00 |  |
| pass | time.query | snapshot/q14 | 132.13 | 51.34 | 0.40 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.16 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | snapshot | 76815.05 | 24430.19 | 0.32 | 2.00 |  |
| pass | time.open | snapshot | 1464.59 | 852.52 | 0.59 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 39.06 | 0.01 | 0.05 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 189.65 | 22.81 | 0.13 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 39.17 | 0.01 | 0.05 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 75.19 | 0.04 | 0.03 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 621.34 | 185.15 | 0.30 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 511.38 | 73.20 | 0.15 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 1147.71 | 203.97 | 0.18 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 871.82 | 218.90 | 0.25 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 1450.56 | 276.69 | 0.19 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 697.13 | 185.21 | 0.27 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 1.49 | 0.14 | 0.61 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 0.72 | 0.06 | 0.76 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 906.09 | 85.79 | 0.10 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 320.35 | 57.71 | 0.19 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.04 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk@materialize | 1627585.07 | 16771.85 | 0.01 | 2.50 |  |
| pass | time.open | disk@materialize | 7673.50 | 1370.17 | 0.18 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 39.06 | 0.02 | 0.05 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 189.65 | 50.03 | 0.27 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 39.17 | 0.01 | 0.05 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 75.19 | 0.06 | 0.03 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 621.34 | 242.13 | 0.39 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 511.38 | 172.05 | 0.34 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 1147.71 | 246.23 | 0.22 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 871.82 | 226.90 | 0.26 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 1450.56 | 460.82 | 0.32 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 697.13 | 193.71 | 0.28 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 1.49 | 0.23 | 0.64 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.72 | 0.15 | 0.79 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 906.09 | 190.73 | 0.21 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 320.35 | 137.77 | 0.43 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.06 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk@stream | 1627585.07 | 16240.10 | 0.01 | 2.50 |  |
| pass | time.open | disk@stream | 7673.50 | 0.81 | 0.00 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 60.72 | 0.03 | 0.03 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 729.49 | 537.81 | 0.74 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 195.54 | 0.06 | 0.01 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 989.95 | 0.24 | 0.00 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 537.67 | 324.91 | 0.61 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 468.29 | 213.12 | 0.46 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 977.60 | 362.61 | 0.37 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 904.88 | 386.81 | 0.43 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 2125.90 | 681.36 | 0.32 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 594.89 | 367.28 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 16.79 | 1.84 | 0.20 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 10.19 | 0.21 | 0.18 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 511.89 | 236.20 | 0.46 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 382.71 | 191.52 | 0.50 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.07 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 277762.61 | 17900.38 | 0.06 | 2.00 |  |
| pass | time.open | rocksdb | 59389.43 | 622.52 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 710.34 | 600.99 | 0.85 | 1.50 |  |
| pass | rss.build_peak | snapshot | 741.02 | 550.09 | 0.75 | 1.50 |  |
| pass | rss.after_open | snapshot | 596.36 | 489.11 | 0.83 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 740.63 | 457.07 | 0.63 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 738.11 | 400.31 | 0.56 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 543.97 | 350.98 | 0.66 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 740.63 | 207.27 | 0.31 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 738.11 | 398.18 | 0.56 | 1.50 |  |
| pass | rss.after_open | disk@stream | 543.97 | 6.13 | 0.07 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 663.27 | 436.19 | 0.67 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 634.20 | 401.63 | 0.65 | 1.50 |  |
| pass | rss.after_open | rocksdb | 522.04 | 270.78 | 0.55 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 889778182.00 | 653959168.00 | 0.73 | 1.25 |  |
| pass | bytes.dir | disk@stream | 889778182.00 | 653959168.00 | 0.73 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206253032.00 | 155661891.00 | 0.75 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 574.14 | 11539.82 | 20.10 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 585.42 | 74691.02 | 127.58 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 17.13 | 6223.20 | 363.36 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 13.41 | 33157.81 | 2472.61 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 596.45 | 5391.49 | 9.04 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 617.81 | 15417.40 | 24.95 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 17.80 | 2504.07 | 140.65 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 15.37 | 11600.33 | 754.93 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 23.33 | 12086.25 | 518.02 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 22.48 | 73472.17 | 3268.44 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 12.60 | 5794.96 | 460.01 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 11.69 | 32814.89 | 2807.77 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 23.30 | 5379.95 | 230.89 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 22.18 | 15604.18 | 703.52 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 11.64 | 2596.51 | 223.01 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 11.47 | 11694.46 | 1019.30 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 23.33 | 10898.00 | 467.09 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 22.48 | 42034.77 | 1869.93 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 12.60 | 4801.15 | 381.12 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 11.69 | 19492.94 | 1667.89 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 23.30 | 4698.58 | 201.65 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 22.18 | 13915.18 | 627.37 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 11.64 | 1967.57 | 168.99 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 11.47 | 9309.44 | 811.42 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 15.07 | 8236.12 | 546.66 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 14.36 | 49920.81 | 3476.00 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 1.20 | 2529.17 | 2114.26 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 1.16 | 10860.02 | 9340.00 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 15.84 | 3781.08 | 238.77 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 15.10 | 11894.63 | 787.67 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 1.21 | 1629.76 | 1347.27 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 1.16 | 6206.68 | 5329.54 | – | report-only |

