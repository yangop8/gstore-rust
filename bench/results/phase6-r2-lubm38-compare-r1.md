## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.25 1.20 2.45
- calibration score 61.44 ms (sort 43.30, hashmap 110.94, format 48.27, fsync 0.52)
- compared with: reference profile, commit `05be7e3d58b27605c7e1eb2680039e52fb961743`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 28.32 | 549.8 | 430.8 | 1148.5 (cold) | 898.0 | 488.6 | 590.4 | 18.28 | 1.09× |
| disk | 19.84 | 400.4 | 654.0 | 1654.4 (cold) | 1582.0 | 350.1 | 460.2 | 37.51 | 0.97× |
| rocksdb | 164.35 | 510.6 | 206.2 | 1443.6 (cold) | 937.4 | 199.8 | 386.4 | 262.79 | 1.13× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.44 | 0.18 | 1.25× | 7.57 | 4.35 | 1.05× | 71.61 | 62.72 | 1.24× |
| q2 | 115 | 42.53 | 39.60 | 1.01× | 50.97 | 48.86 | 0.99× | 1176.15 | 1152.14 | 1.12× |
| q3 | 10 | 0.86 | 0.61 | 1.24× | 6.34 | 4.61 | 0.95× | 201.13 | 185.81 | 1.06× |
| q4 | 9 | 0.17 | 0.11 | 1.13× | 2.70 | 2.09 | 1.01× | 430.83 | 438.05 | 1.11× |
| q5 | 576 | 285.91 | 228.92 | 1.16× | 313.00 | 242.15 | 0.85× | 481.68 | 465.94 | 1.19× |
| q6 | 404399 | 124.47 | 124.71 | 1.10× | 151.66 | 153.32 | 1.02× | 346.89 | 346.18 | 1.22× |
| q7 | 23 | 267.00 | 279.15 | 1.09× | 366.56 | 344.47 | 0.91× | 765.15 | 763.28 | 0.99× |
| q8 | 10778 | 246.03 | 234.27 | 1.04× | 326.31 | 299.37 | 0.94× | 709.61 | 733.13 | 1.10× |
| q9 | 5507 | 326.73 | 336.80 | 1.07× | 419.18 | 453.39 | 1.02× | 1921.31 | 1900.32 | 1.00× |
| q10 | 3 | 225.21 | 229.32 | 1.10× | 258.86 | 245.81 | 0.90× | 415.09 | 436.99 | 0.97× |
| q11 | 325 | 2.41 | 1.12 | 1.00× | 2.55 | 1.00 | 1.05× | 24.57 | 16.99 | 1.20× |
| q12 | 21 | 1.06 | 0.46 | 0.92× | 1.84 | 0.53 | 0.92× | 13.29 | 10.85 | 1.34× |
| q13 | 432512 | 141.34 | 142.92 | 1.06× | 215.71 | 215.47 | 0.97× | 344.21 | 347.93 | 1.14× |
| q14 | 305503 | 88.44 | 89.57 | 1.10× | 111.27 | 109.37 | 1.02× | 258.12 | 264.89 | 1.23× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.02 / 0.19 | 0.00 / 0.01 | 6478 | 55.8 | 33.4 | 1369.5 | 848.3 | 842657 → 843783 | ok |
| disk | 1.53 / 2.67 | 1.60 / 2.41 | 6478 | 1297.9 | 1177.0 | 0.0 | 3086.5 | 842657 → 843783 | ok |
| rocksdb | 0.21 / 0.37 | 0.17 / 0.29 | 6478 | 192.4 | 165.0 | 34.5 | 920.1 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 2941.9 | 0.34 | 0.51 | 0 | identical | 0.75× |
| snapshot | sparql | q1 | 8 | 2868.8 | 2.79 | 3.78 | 0 | identical | 0.76× |
| snapshot | sparql | q4 | 1 | 3734.0 | 0.26 | 0.40 | 0 | identical | 0.88× |
| snapshot | sparql | q4 | 8 | 4658.0 | 1.71 | 2.93 | 0 | identical | 0.96× |
| snapshot | api | q1 | 1 | 2068.5 | 0.47 | 0.71 | 0 | identical | 0.90× |
| snapshot | api | q1 | 8 | 3065.9 | 2.59 | 5.01 | 0 | identical | 0.83× |
| snapshot | api | q4 | 1 | 1989.4 | 0.45 | 0.72 | 0 | identical | 0.99× |
| snapshot | api | q4 | 8 | 4556.2 | 1.70 | 3.25 | 0 | identical | 1.16× |
| disk | sparql | q1 | 1 | 183.5 | 5.45 | 7.27 | 0 | identical | 0.98× |
| disk | sparql | q1 | 8 | 150.5 | 52.87 | 64.43 | 0 | identical | 0.98× |
| disk | sparql | q4 | 1 | 338.2 | 2.95 | 3.89 | 0 | identical | 0.92× |
| disk | sparql | q4 | 8 | 277.4 | 28.77 | 34.86 | 0 | identical | 0.98× |
| disk | api | q1 | 1 | 173.7 | 5.74 | 7.98 | 0 | identical | 0.95× |
| disk | api | q1 | 8 | 135.2 | 58.94 | 69.84 | 0 | identical | 0.94× |
| disk | api | q4 | 1 | 308.6 | 3.18 | 4.37 | 0 | identical | 0.95× |
| disk | api | q4 | 8 | 228.9 | 34.76 | 47.49 | 0 | identical | 0.91× |
| rocksdb | sparql | q1 | 1 | 18.4 | 54.36 | 83.20 | 0 | identical | 1.13× |
| rocksdb | sparql | q1 | 8 | 15.7 | 490.86 | 544.34 | 0 | identical | 1.00× |
| rocksdb | sparql | q4 | 1 | 2.5 | 402.00 | 590.91 | 0 | identical | 0.97× |
| rocksdb | sparql | q4 | 8 | 2.3 | 2824.52 | 3639.97 | 0 | identical | 0.96× |
| rocksdb | api | q1 | 1 | 18.8 | 53.00 | 64.11 | 0 | identical | 1.00× |
| rocksdb | api | q1 | 8 | 15.9 | 482.48 | 565.42 | 0 | identical | 1.00× |
| rocksdb | api | q4 | 1 | 2.6 | 383.15 | 487.18 | 0 | identical | 1.24× |
| rocksdb | api | q4 | 8 | 2.5 | 2654.04 | 3351.52 | 0 | identical | 1.02× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.14 | 0.17 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q2 | 39.17 | 37.49 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.49 | 0.58 | 1.03 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.10 | 0.11 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q5 | 197.77 | 216.74 | 1.09 | 3.00 |  |
| pass | time.query | snapshot/q6 | 113.37 | 118.08 | 1.04 | 3.00 |  |
| pass | time.query | snapshot/q7 | 255.58 | 264.30 | 1.03 | 3.00 |  |
| pass | time.query | snapshot/q8 | 225.03 | 221.80 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q9 | 314.47 | 318.88 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q10 | 207.92 | 217.12 | 1.04 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.12 | 1.06 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.50 | 0.44 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q13 | 134.62 | 135.31 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q14 | 81.26 | 84.81 | 1.04 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 1.02 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 28923.11 | 26811.45 | 0.93 | 2.00 |  |
| pass | time.open | snapshot | 1561.48 | 850.25 | 0.55 | 2.50 |  |
| pass | time.query | disk/q1 | 4.13 | 4.11 | 1.00 | 3.00 |  |
| pass | time.query | disk/q2 | 49.51 | 46.26 | 0.94 | 3.00 |  |
| pass | time.query | disk/q3 | 4.85 | 4.36 | 0.93 | 3.00 |  |
| pass | time.query | disk/q4 | 2.08 | 1.98 | 0.98 | 3.00 |  |
| pass | time.query | disk/q5 | 284.04 | 229.26 | 0.81 | 3.00 |  |
| pass | time.query | disk/q6 | 150.04 | 145.16 | 0.97 | 3.00 |  |
| pass | time.query | disk/q7 | 380.44 | 326.14 | 0.86 | 3.00 |  |
| pass | time.query | disk/q8 | 317.11 | 283.44 | 0.89 | 3.00 |  |
| pass | time.query | disk/q9 | 445.11 | 429.27 | 0.96 | 3.00 |  |
| pass | time.query | disk/q10 | 272.01 | 232.73 | 0.86 | 3.00 |  |
| pass | time.query | disk/q11 | 0.95 | 0.95 | 1.00 | 3.00 |  |
| pass | time.query | disk/q12 | 0.58 | 0.50 | 0.97 | 3.00 |  |
| pass | time.query | disk/q13 | 221.41 | 204.00 | 0.92 | 3.00 |  |
| pass | time.query | disk/q14 | 106.80 | 103.55 | 0.97 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.92 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | disk | 575884.40 | 18788.29 | 0.03 | 2.50 |  |
| pass | time.open | disk | 2091.80 | 1497.81 | 0.72 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 50.40 | 51.05 | 1.01 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 1030.58 | 937.74 | 0.91 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 174.64 | 151.23 | 0.87 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 394.47 | 356.54 | 0.90 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 391.99 | 379.24 | 0.97 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 282.88 | 281.76 | 1.00 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 774.74 | 621.24 | 0.80 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 668.82 | 596.70 | 0.89 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 1895.52 | 1546.69 | 0.82 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 451.43 | 355.67 | 0.79 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 14.16 | 13.83 | 0.98 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 8.11 | 8.83 | 1.07 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 305.54 | 283.18 | 0.93 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 215.50 | 215.59 | 1.00 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.92 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 175933.02 | 133767.01 | 0.76 | 2.00 |  |
| pass | time.open | rocksdb | 1186.80 | 762.97 | 0.65 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 708.27 | 590.36 | 0.84 | 1.50 |  |
| pass | rss.build_peak | snapshot | 670.05 | 549.84 | 0.83 | 1.50 |  |
| pass | rss.after_open | snapshot | 608.92 | 488.62 | 0.81 | 1.50 |  |
| pass | rss.query_hwm | disk | 580.53 | 460.23 | 0.80 | 1.50 |  |
| pass | rss.build_peak | disk | 527.07 | 400.41 | 0.77 | 1.50 |  |
| pass | rss.after_open | disk | 470.59 | 350.09 | 0.76 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 508.07 | 386.38 | 0.77 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 628.26 | 510.58 | 0.82 | 1.50 |  |
| pass | rss.after_open | rocksdb | 315.07 | 199.84 | 0.67 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 889778181.00 | 653959174.00 | 0.73 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206222953.00 | 206221265.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 3901.60 | 2941.93 | 0.75 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 3771.42 | 2868.76 | 0.76 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 4247.28 | 3733.98 | 0.88 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 4839.28 | 4657.96 | 0.96 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 2300.11 | 2068.46 | 0.90 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 3709.74 | 3065.87 | 0.83 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2008.79 | 1989.42 | 0.99 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 3923.69 | 4556.24 | 1.16 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 187.87 | 183.45 | 0.98 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 154.03 | 150.47 | 0.98 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 366.48 | 338.24 | 0.92 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 283.97 | 277.41 | 0.98 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 182.90 | 173.67 | 0.95 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 144.19 | 135.22 | 0.94 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 324.03 | 308.55 | 0.95 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 251.48 | 228.93 | 0.91 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 16.31 | 18.39 | 1.13 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 15.69 | 15.67 | 1.00 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 2.57 | 2.49 | 0.97 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 2.37 | 2.28 | 0.96 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 18.81 | 18.84 | 1.00 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 15.80 | 15.87 | 1.00 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 2.11 | 2.61 | 1.24 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 2.42 | 2.48 | 1.02 | – | report-only |

