## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `05be7e3d58b27605c7e1eb2680039e52fb961743`, linux x86_64 × 4 CPUs, 16091 MiB, load 1.00 1.09 1.85
- calibration score 58.17 ms (sort 42.44, hashmap 95.70, format 48.46, fsync 0.29)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 28.92 | 670.0 | 430.8 | 1590.5 (cold) | 1561.5 | 608.9 | 708.3 | 16.79 | 0.36× |
| disk | 575.88 | 527.1 | 889.8 | 5712.3 (cold) | 2091.8 | 470.6 | 580.5 | 38.65 | 0.26× |
| rocksdb | 175.93 | 628.3 | 206.2 | 1224.4 (cold) | 1186.8 | 315.1 | 508.1 | 232.27 | 0.75× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.38 | 0.14 | 0.11× | 7.41 | 4.13 | 0.11× | 57.48 | 50.40 | 0.83× |
| q2 | 115 | 42.45 | 39.17 | 0.77× | 57.08 | 49.51 | 0.26× | 1139.12 | 1030.58 | 1.41× |
| q3 | 10 | 0.69 | 0.49 | 0.22× | 6.96 | 4.85 | 0.12× | 184.61 | 174.64 | 0.89× |
| q4 | 9 | 0.16 | 0.10 | 0.00× | 2.69 | 2.08 | 0.03× | 436.28 | 394.47 | 0.40× |
| q5 | 576 | 228.81 | 197.77 | 0.59× | 281.94 | 284.04 | 0.46× | 422.91 | 391.99 | 0.73× |
| q6 | 404399 | 118.26 | 113.37 | 0.60× | 158.96 | 150.04 | 0.29× | 309.88 | 282.88 | 0.60× |
| q7 | 23 | 254.79 | 255.58 | 0.63× | 389.62 | 380.44 | 0.33× | 753.61 | 774.74 | 0.79× |
| q8 | 10778 | 217.02 | 225.03 | 0.62× | 388.21 | 317.11 | 0.36× | 710.19 | 668.82 | 0.74× |
| q9 | 5507 | 312.44 | 314.47 | 0.59× | 456.27 | 445.11 | 0.31× | 1801.15 | 1895.52 | 0.89× |
| q10 | 3 | 192.75 | 207.92 | 0.64× | 246.90 | 272.01 | 0.39× | 402.86 | 451.43 | 0.76× |
| q11 | 325 | 2.33 | 1.12 | 1.04× | 2.48 | 0.95 | 0.64× | 18.26 | 14.16 | 0.84× |
| q12 | 21 | 1.09 | 0.50 | 0.86× | 1.87 | 0.58 | 0.80× | 9.83 | 8.11 | 0.80× |
| q13 | 432512 | 131.99 | 134.62 | 0.60× | 217.42 | 221.41 | 0.24× | 318.01 | 305.54 | 0.60× |
| q14 | 305503 | 80.09 | 81.26 | 0.61× | 108.83 | 106.80 | 0.33× | 219.91 | 215.50 | 0.56× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.02 / 0.21 | 0.00 / 0.01 | 6478 | 60.9 | 35.2 | 1309.4 | 1198.2 | 842657 → 843783 | ok |
| disk | 1.19 / 1.58 | 0.93 / 1.26 | 6478 | 1056.6 | 1289.6 | 0.0 | 3400.0 | 842657 → 843783 | ok |
| rocksdb | 0.26 / 0.47 | 0.21 / 0.28 | 6478 | 187.7 | 123.8 | 35.7 | 903.3 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 3901.6 | 0.26 | 0.40 | 0 | identical | 6.80× |
| snapshot | sparql | q1 | 8 | 3771.4 | 2.12 | 2.71 | 0 | identical | 6.44× |
| snapshot | sparql | q4 | 1 | 4247.3 | 0.23 | 0.36 | 0 | identical | 247.99× |
| snapshot | sparql | q4 | 8 | 4839.3 | 1.65 | 2.68 | 0 | identical | 360.87× |
| snapshot | api | q1 | 1 | 2300.1 | 0.42 | 0.60 | 0 | identical | 3.86× |
| snapshot | api | q1 | 8 | 3709.7 | 2.14 | 4.16 | 0 | identical | 6.00× |
| snapshot | api | q4 | 1 | 2008.8 | 0.45 | 0.66 | 0 | identical | 112.83× |
| snapshot | api | q4 | 8 | 3923.7 | 1.97 | 3.68 | 0 | identical | 255.35× |
| disk | sparql | q1 | 1 | 187.9 | 5.32 | 7.11 | 0 | identical | 8.05× |
| disk | sparql | q1 | 8 | 154.0 | 51.72 | 61.11 | 0 | identical | 6.85× |
| disk | sparql | q4 | 1 | 366.5 | 2.72 | 3.62 | 0 | identical | 29.09× |
| disk | sparql | q4 | 8 | 284.0 | 28.08 | 33.39 | 0 | identical | 24.30× |
| disk | api | q1 | 1 | 182.9 | 5.45 | 7.45 | 0 | identical | 7.85× |
| disk | api | q1 | 8 | 144.2 | 55.22 | 64.44 | 0 | identical | 6.50× |
| disk | api | q4 | 1 | 324.0 | 3.03 | 3.96 | 0 | identical | 27.83× |
| disk | api | q4 | 8 | 251.5 | 31.67 | 36.44 | 0 | identical | 21.92× |
| rocksdb | sparql | q1 | 1 | 16.3 | 61.30 | 85.86 | 0 | identical | 1.08× |
| rocksdb | sparql | q1 | 8 | 15.7 | 489.34 | 567.58 | 0 | identical | 1.09× |
| rocksdb | sparql | q4 | 1 | 2.6 | 389.78 | 462.61 | 0 | identical | 2.14× |
| rocksdb | sparql | q4 | 8 | 2.4 | 2777.08 | 3600.23 | 0 | identical | 2.04× |
| rocksdb | api | q1 | 1 | 18.8 | 53.12 | 75.39 | 0 | identical | 1.19× |
| rocksdb | api | q1 | 8 | 15.8 | 487.61 | 574.46 | 0 | identical | 1.05× |
| rocksdb | api | q4 | 1 | 2.1 | 473.79 | 580.57 | 0 | identical | 1.74× |
| rocksdb | api | q4 | 8 | 2.4 | 2758.51 | 3695.39 | 0 | identical | 2.08× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 1.30 | 0.15 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q2 | 50.80 | 42.14 | 0.84 | 3.00 |  |
| pass | time.query | snapshot/q3 | 2.19 | 0.53 | 0.60 | 3.00 |  |
| pass | time.query | snapshot/q4 | 53.63 | 0.11 | 0.04 | 3.00 |  |
| pass | time.query | snapshot/q5 | 333.25 | 212.79 | 0.64 | 3.00 |  |
| pass | time.query | snapshot/q6 | 187.61 | 121.98 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q7 | 402.96 | 274.98 | 0.68 | 3.00 |  |
| pass | time.query | snapshot/q8 | 363.48 | 242.11 | 0.67 | 3.00 |  |
| pass | time.query | snapshot/q9 | 529.28 | 338.34 | 0.64 | 3.00 |  |
| pass | time.query | snapshot/q10 | 326.63 | 223.70 | 0.69 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.09 | 1.21 | 1.04 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.59 | 0.54 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q13 | 223.92 | 144.83 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q14 | 132.13 | 87.43 | 0.67 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.36 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | snapshot | 76815.05 | 31118.55 | 0.41 | 2.00 |  |
| pass | time.open | snapshot | 1464.59 | 1680.01 | 1.15 | 2.50 |  |
| pass | time.query | disk/q1 | 39.06 | 4.45 | 0.16 | 3.00 |  |
| pass | time.query | disk/q2 | 189.65 | 53.27 | 0.29 | 3.00 |  |
| pass | time.query | disk/q3 | 39.17 | 5.22 | 0.18 | 3.00 |  |
| pass | time.query | disk/q4 | 75.19 | 2.24 | 0.05 | 3.00 |  |
| pass | time.query | disk/q5 | 621.34 | 305.60 | 0.49 | 3.00 |  |
| pass | time.query | disk/q6 | 511.38 | 161.43 | 0.32 | 3.00 |  |
| pass | time.query | disk/q7 | 1147.71 | 409.31 | 0.36 | 3.00 |  |
| pass | time.query | disk/q8 | 871.82 | 341.18 | 0.39 | 3.00 |  |
| pass | time.query | disk/q9 | 1450.56 | 478.90 | 0.33 | 3.00 |  |
| pass | time.query | disk/q10 | 697.13 | 292.65 | 0.42 | 3.00 |  |
| pass | time.query | disk/q11 | 1.49 | 1.02 | 0.87 | 3.00 |  |
| pass | time.query | disk/q12 | 0.72 | 0.62 | 0.96 | 3.00 |  |
| pass | time.query | disk/q13 | 906.09 | 238.22 | 0.26 | 3.00 |  |
| pass | time.query | disk/q14 | 320.35 | 114.91 | 0.36 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.26 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk | 1627585.07 | 619597.47 | 0.38 | 2.50 |  |
| pass | time.open | disk | 7673.50 | 2250.58 | 0.30 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 60.72 | 51.76 | 0.86 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 729.49 | 1058.34 | 1.45 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 195.54 | 179.34 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 989.95 | 405.10 | 0.41 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 537.67 | 402.55 | 0.75 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 468.29 | 290.50 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 977.60 | 795.61 | 0.81 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 904.88 | 686.83 | 0.76 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 2125.90 | 1946.58 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 594.89 | 463.59 | 0.78 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 16.79 | 14.54 | 0.88 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 10.19 | 8.33 | 0.85 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 511.89 | 313.78 | 0.61 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 382.71 | 221.31 | 0.58 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.77 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 277762.61 | 180672.20 | 0.65 | 2.00 |  |
| pass | time.open | rocksdb | 59389.43 | 1218.77 | 0.02 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 710.34 | 708.27 | 1.00 | 1.50 |  |
| pass | rss.build_peak | snapshot | 741.02 | 670.05 | 0.91 | 1.50 |  |
| pass | rss.after_open | snapshot | 596.36 | 608.92 | 1.02 | 1.50 |  |
| pass | rss.query_hwm | disk | 740.63 | 580.53 | 0.79 | 1.50 |  |
| pass | rss.build_peak | disk | 738.11 | 527.07 | 0.73 | 1.50 |  |
| pass | rss.after_open | disk | 543.97 | 470.59 | 0.87 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 663.27 | 508.07 | 0.78 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 634.20 | 628.26 | 0.99 | 1.50 |  |
| pass | rss.after_open | rocksdb | 522.04 | 315.07 | 0.63 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 889778182.00 | 889778181.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206253032.00 | 206222953.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 574.14 | 3901.60 | 6.80 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 585.42 | 3771.42 | 6.44 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 17.13 | 4247.28 | 247.99 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 13.41 | 4839.28 | 360.87 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 596.45 | 2300.11 | 3.86 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 617.81 | 3709.74 | 6.00 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 17.80 | 2008.79 | 112.83 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 15.37 | 3923.69 | 255.35 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 23.33 | 187.87 | 8.05 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 22.48 | 154.03 | 6.85 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 12.60 | 366.48 | 29.09 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 11.69 | 283.97 | 24.30 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 23.30 | 182.90 | 7.85 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 22.18 | 144.19 | 6.50 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 11.64 | 324.03 | 27.83 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 11.47 | 251.48 | 21.92 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 15.07 | 16.31 | 1.08 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 14.36 | 15.69 | 1.09 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 1.20 | 2.57 | 2.14 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 1.16 | 2.37 | 2.04 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 15.84 | 18.81 | 1.19 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 15.10 | 15.80 | 1.05 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 1.21 | 2.11 | 1.74 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 1.16 | 2.42 | 2.08 | – | report-only |

