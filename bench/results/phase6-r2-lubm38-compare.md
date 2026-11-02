## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.25 1.20 2.45
- calibration score 61.44 ms (sort 43.30, hashmap 110.94, format 48.27, fsync 0.52)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 28.32 | 549.8 | 430.8 | 1148.5 (cold) | 898.0 | 488.6 | 590.4 | 18.28 | 0.39× |
| disk | 19.84 | 400.4 | 654.0 | 1654.4 (cold) | 1582.0 | 350.1 | 460.2 | 37.51 | 0.26× |
| rocksdb | 164.35 | 510.6 | 206.2 | 1443.6 (cold) | 937.4 | 199.8 | 386.4 | 262.79 | 0.84× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.44 | 0.18 | 0.14× | 7.57 | 4.35 | 0.11× | 71.61 | 62.72 | 1.03× |
| q2 | 115 | 42.53 | 39.60 | 0.78× | 50.97 | 48.86 | 0.26× | 1176.15 | 1152.14 | 1.58× |
| q3 | 10 | 0.86 | 0.61 | 0.28× | 6.34 | 4.61 | 0.12× | 201.13 | 185.81 | 0.95× |
| q4 | 9 | 0.17 | 0.11 | 0.00× | 2.70 | 2.09 | 0.03× | 430.83 | 438.05 | 0.44× |
| q5 | 576 | 285.91 | 228.92 | 0.69× | 313.00 | 242.15 | 0.39× | 481.68 | 465.94 | 0.87× |
| q6 | 404399 | 124.47 | 124.71 | 0.66× | 151.66 | 153.32 | 0.30× | 346.89 | 346.18 | 0.74× |
| q7 | 23 | 267.00 | 279.15 | 0.69× | 366.56 | 344.47 | 0.30× | 765.15 | 763.28 | 0.78× |
| q8 | 10778 | 246.03 | 234.27 | 0.64× | 326.31 | 299.37 | 0.34× | 709.61 | 733.13 | 0.81× |
| q9 | 5507 | 326.73 | 336.80 | 0.64× | 419.18 | 453.39 | 0.31× | 1921.31 | 1900.32 | 0.89× |
| q10 | 3 | 225.21 | 229.32 | 0.70× | 258.86 | 245.81 | 0.35× | 415.09 | 436.99 | 0.73× |
| q11 | 325 | 2.41 | 1.12 | 1.04× | 2.55 | 1.00 | 0.67× | 24.57 | 16.99 | 1.01× |
| q12 | 21 | 1.06 | 0.46 | 0.79× | 1.84 | 0.53 | 0.74× | 13.29 | 10.85 | 1.06× |
| q13 | 432512 | 141.34 | 142.92 | 0.64× | 215.71 | 215.47 | 0.24× | 344.21 | 347.93 | 0.68× |
| q14 | 305503 | 88.44 | 89.57 | 0.68× | 111.27 | 109.37 | 0.34× | 258.12 | 264.89 | 0.69× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.02 / 0.19 | 0.00 / 0.01 | 6478 | 55.8 | 33.4 | 1369.5 | 848.3 | 842657 → 843783 | ok |
| disk | 1.53 / 2.67 | 1.60 / 2.41 | 6478 | 1297.9 | 1177.0 | 0.0 | 3086.5 | 842657 → 843783 | ok |
| rocksdb | 0.21 / 0.37 | 0.17 / 0.29 | 6478 | 192.4 | 165.0 | 34.5 | 920.1 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 2941.9 | 0.34 | 0.51 | 0 | identical | 5.12× |
| snapshot | sparql | q1 | 8 | 2868.8 | 2.79 | 3.78 | 0 | identical | 4.90× |
| snapshot | sparql | q4 | 1 | 3734.0 | 0.26 | 0.40 | 0 | identical | 218.02× |
| snapshot | sparql | q4 | 8 | 4658.0 | 1.71 | 2.93 | 0 | identical | 347.35× |
| snapshot | api | q1 | 1 | 2068.5 | 0.47 | 0.71 | 0 | identical | 3.47× |
| snapshot | api | q1 | 8 | 3065.9 | 2.59 | 5.01 | 0 | identical | 4.96× |
| snapshot | api | q4 | 1 | 1989.4 | 0.45 | 0.72 | 0 | identical | 111.75× |
| snapshot | api | q4 | 8 | 4556.2 | 1.70 | 3.25 | 0 | identical | 296.51× |
| disk | sparql | q1 | 1 | 183.5 | 5.45 | 7.27 | 0 | identical | 7.86× |
| disk | sparql | q1 | 8 | 150.5 | 52.87 | 64.43 | 0 | identical | 6.69× |
| disk | sparql | q4 | 1 | 338.2 | 2.95 | 3.89 | 0 | identical | 26.85× |
| disk | sparql | q4 | 8 | 277.4 | 28.77 | 34.86 | 0 | identical | 23.74× |
| disk | api | q1 | 1 | 173.7 | 5.74 | 7.98 | 0 | identical | 7.45× |
| disk | api | q1 | 8 | 135.2 | 58.94 | 69.84 | 0 | identical | 6.10× |
| disk | api | q4 | 1 | 308.6 | 3.18 | 4.37 | 0 | identical | 26.50× |
| disk | api | q4 | 8 | 228.9 | 34.76 | 47.49 | 0 | identical | 19.95× |
| rocksdb | sparql | q1 | 1 | 18.4 | 54.36 | 83.20 | 0 | identical | 1.22× |
| rocksdb | sparql | q1 | 8 | 15.7 | 490.86 | 544.34 | 0 | identical | 1.09× |
| rocksdb | sparql | q4 | 1 | 2.5 | 402.00 | 590.91 | 0 | identical | 2.08× |
| rocksdb | sparql | q4 | 8 | 2.3 | 2824.52 | 3639.97 | 0 | identical | 1.96× |
| rocksdb | api | q1 | 1 | 18.8 | 53.00 | 64.11 | 0 | identical | 1.19× |
| rocksdb | api | q1 | 8 | 15.9 | 482.48 | 565.42 | 0 | identical | 1.05× |
| rocksdb | api | q4 | 1 | 2.6 | 383.15 | 487.18 | 0 | identical | 2.16× |
| rocksdb | api | q4 | 8 | 2.5 | 2654.04 | 3351.52 | 0 | identical | 2.13× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 1.30 | 0.18 | 0.66 | 3.00 |  |
| pass | time.query | snapshot/q2 | 50.80 | 40.34 | 0.80 | 3.00 |  |
| pass | time.query | snapshot/q3 | 2.19 | 0.62 | 0.63 | 3.00 |  |
| pass | time.query | snapshot/q4 | 53.63 | 0.11 | 0.04 | 3.00 |  |
| pass | time.query | snapshot/q5 | 333.25 | 233.19 | 0.70 | 3.00 |  |
| pass | time.query | snapshot/q6 | 187.61 | 127.04 | 0.68 | 3.00 |  |
| pass | time.query | snapshot/q7 | 402.96 | 284.36 | 0.71 | 3.00 |  |
| pass | time.query | snapshot/q8 | 363.48 | 238.64 | 0.66 | 3.00 |  |
| pass | time.query | snapshot/q9 | 529.28 | 343.08 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q10 | 326.63 | 233.60 | 0.72 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.09 | 1.15 | 1.02 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.59 | 0.47 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q13 | 223.92 | 145.58 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q14 | 132.13 | 91.25 | 0.70 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.38 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | snapshot | 76815.05 | 28846.59 | 0.38 | 2.00 |  |
| pass | time.open | snapshot | 1464.59 | 914.79 | 0.63 | 2.50 |  |
| pass | time.query | disk/q1 | 39.06 | 4.43 | 0.16 | 3.00 |  |
| pass | time.query | disk/q2 | 189.65 | 49.77 | 0.27 | 3.00 |  |
| pass | time.query | disk/q3 | 39.17 | 4.69 | 0.16 | 3.00 |  |
| pass | time.query | disk/q4 | 75.19 | 2.13 | 0.05 | 3.00 |  |
| pass | time.query | disk/q5 | 621.34 | 246.66 | 0.40 | 3.00 |  |
| pass | time.query | disk/q6 | 511.38 | 156.18 | 0.31 | 3.00 |  |
| pass | time.query | disk/q7 | 1147.71 | 350.90 | 0.31 | 3.00 |  |
| pass | time.query | disk/q8 | 871.82 | 304.96 | 0.35 | 3.00 |  |
| pass | time.query | disk/q9 | 1450.56 | 461.85 | 0.32 | 3.00 |  |
| pass | time.query | disk/q10 | 697.13 | 250.39 | 0.36 | 3.00 |  |
| pass | time.query | disk/q11 | 1.49 | 1.02 | 0.87 | 3.00 |  |
| pass | time.query | disk/q12 | 0.72 | 0.54 | 0.93 | 3.00 |  |
| pass | time.query | disk/q13 | 906.09 | 219.48 | 0.24 | 3.00 |  |
| pass | time.query | disk/q14 | 320.35 | 111.41 | 0.35 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.24 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk | 1627585.07 | 20214.43 | 0.01 | 2.50 |  |
| pass | time.open | disk | 7673.50 | 1611.51 | 0.21 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 60.72 | 52.42 | 0.87 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 729.49 | 963.00 | 1.32 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 195.54 | 155.30 | 0.80 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 989.95 | 366.14 | 0.37 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 537.67 | 389.45 | 0.73 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 468.29 | 289.35 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 977.60 | 637.97 | 0.65 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 904.88 | 612.78 | 0.68 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 2125.90 | 1588.35 | 0.75 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 594.89 | 365.25 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 16.79 | 14.20 | 0.86 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 10.19 | 9.07 | 0.91 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 511.89 | 290.81 | 0.57 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 382.71 | 221.40 | 0.58 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.71 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 277762.61 | 137370.35 | 0.49 | 2.00 |  |
| pass | time.open | rocksdb | 59389.43 | 783.52 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 710.34 | 590.36 | 0.84 | 1.50 |  |
| pass | rss.build_peak | snapshot | 741.02 | 549.84 | 0.75 | 1.50 |  |
| pass | rss.after_open | snapshot | 596.36 | 488.62 | 0.83 | 1.50 |  |
| pass | rss.query_hwm | disk | 740.63 | 460.23 | 0.64 | 1.50 |  |
| pass | rss.build_peak | disk | 738.11 | 400.41 | 0.56 | 1.50 |  |
| pass | rss.after_open | disk | 543.97 | 350.09 | 0.66 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 663.27 | 386.38 | 0.60 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 634.20 | 510.58 | 0.81 | 1.50 |  |
| pass | rss.after_open | rocksdb | 522.04 | 199.84 | 0.42 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 889778182.00 | 653959174.00 | 0.73 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206253032.00 | 206221265.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 574.14 | 2941.93 | 5.12 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 585.42 | 2868.76 | 4.90 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 17.13 | 3733.98 | 218.02 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 13.41 | 4657.96 | 347.35 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 596.45 | 2068.46 | 3.47 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 617.81 | 3065.87 | 4.96 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 17.80 | 1989.42 | 111.75 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 15.37 | 4556.24 | 296.51 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 23.33 | 183.45 | 7.86 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 22.48 | 150.47 | 6.69 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 12.60 | 338.24 | 26.85 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 11.69 | 277.41 | 23.74 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 23.30 | 173.67 | 7.45 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 22.18 | 135.22 | 6.10 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 11.64 | 308.55 | 26.50 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 11.47 | 228.93 | 19.95 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 15.07 | 18.39 | 1.22 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 14.36 | 15.67 | 1.09 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 1.20 | 2.49 | 2.08 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 1.16 | 2.28 | 1.96 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 15.84 | 18.84 | 1.19 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 15.10 | 15.87 | 1.05 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 1.21 | 2.61 | 2.16 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 1.16 | 2.48 | 2.13 | – | report-only |

