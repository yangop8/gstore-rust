## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 1.20 1.41 1.51
- calibration score 60.65 ms (sort 46.57, hashmap 104.98, format 45.63, fsync 0.46)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 28.84 | 550.0 | 430.8 | 1042.9 (cold) | 1015.1 | 489.0 | 588.5 | 16.14 | 0.35× |
| disk | 21.01 | 434.4 | 654.0 | 1491.2 (cold) | 1407.2 | 350.7 | 460.7 | 39.77 | 0.27× |
| rocksdb | 22.25 | 401.6 | 155.9 | 1005.8 (cold) | 900.7 | 276.6 | 440.2 | 66.91 | 0.21× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.36 | 0.10 | 0.08× | 7.78 | 4.22 | 0.11× | 11.52 | 9.43 | 0.16× |
| q2 | 115 | 40.48 | 38.03 | 0.75× | 51.41 | 51.03 | 0.27× | 550.84 | 589.62 | 0.81× |
| q3 | 10 | 0.59 | 0.40 | 0.18× | 7.16 | 4.63 | 0.12× | 36.08 | 30.74 | 0.16× |
| q4 | 9 | 0.15 | 0.10 | 0.00× | 2.70 | 2.12 | 0.03× | 1.94 | 1.53 | 0.00× |
| q5 | 576 | 233.18 | 199.00 | 0.60× | 317.55 | 264.42 | 0.43× | 432.71 | 355.15 | 0.66× |
| q6 | 404399 | 119.18 | 122.03 | 0.65× | 157.30 | 163.68 | 0.32× | 231.35 | 239.14 | 0.51× |
| q7 | 23 | 259.70 | 263.02 | 0.65× | 376.97 | 358.48 | 0.31× | 381.96 | 379.34 | 0.39× |
| q8 | 10778 | 226.49 | 217.78 | 0.60× | 318.06 | 371.78 | 0.43× | 391.19 | 403.36 | 0.45× |
| q9 | 5507 | 305.09 | 307.39 | 0.58× | 435.78 | 490.84 | 0.34× | 741.45 | 740.81 | 0.35× |
| q10 | 3 | 191.32 | 194.07 | 0.59× | 327.43 | 278.67 | 0.40× | 337.99 | 381.82 | 0.64× |
| q11 | 325 | 2.26 | 1.08 | 1.00× | 2.50 | 0.96 | 0.65× | 8.90 | 2.15 | 0.13× |
| q12 | 21 | 1.07 | 0.48 | 0.82× | 1.90 | 0.55 | 0.76× | 0.44 | 0.31 | 0.03× |
| q13 | 432512 | 138.65 | 140.12 | 0.63× | 229.10 | 238.14 | 0.26× | 257.71 | 266.48 | 0.52× |
| q14 | 305503 | 84.81 | 87.90 | 0.67× | 110.19 | 118.82 | 0.37× | 197.42 | 208.40 | 0.54× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.02 / 0.20 | 0.00 / 0.01 | 6478 | 54.9 | 29.9 | 1345.7 | 835.2 | 842657 → 843783 | ok |
| disk | 1.35 / 2.35 | 1.30 / 1.98 | 6478 | 1100.1 | 1042.0 | 0.0 | 2816.7 | 842657 → 843783 | ok |
| rocksdb | 0.22 / 0.37 | 0.21 / 0.34 | 6478 | 153.3 | 191.7 | 37.2 | 730.3 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 5139.5 | 0.19 | 0.32 | 0 | identical | 8.95× |
| snapshot | sparql | q1 | 8 | 25106.4 | 0.31 | 0.65 | 0 | identical | 42.89× |
| snapshot | sparql | q4 | 1 | 4404.4 | 0.22 | 0.36 | 0 | identical | 257.16× |
| snapshot | sparql | q4 | 8 | 23334.2 | 0.34 | 0.70 | 0 | identical | 1740.06× |
| snapshot | api | q1 | 1 | 2972.5 | 0.32 | 0.51 | 0 | identical | 4.98× |
| snapshot | api | q1 | 8 | 10444.9 | 0.75 | 1.52 | 0 | identical | 16.91× |
| snapshot | api | q4 | 1 | 2373.4 | 0.38 | 0.59 | 0 | identical | 133.31× |
| snapshot | api | q4 | 8 | 10506.2 | 0.71 | 1.49 | 0 | identical | 683.73× |
| disk | sparql | q1 | 1 | 195.6 | 5.11 | 6.87 | 0 | identical | 8.39× |
| disk | sparql | q1 | 8 | 444.2 | 18.00 | 26.93 | 0 | identical | 19.76× |
| disk | sparql | q4 | 1 | 381.5 | 2.62 | 3.46 | 0 | identical | 30.28× |
| disk | sparql | q4 | 8 | 920.7 | 8.68 | 11.39 | 0 | identical | 78.78× |
| disk | api | q1 | 1 | 200.7 | 4.96 | 6.46 | 0 | identical | 8.62× |
| disk | api | q1 | 8 | 416.7 | 19.14 | 30.65 | 0 | identical | 18.79× |
| disk | api | q4 | 1 | 355.5 | 2.76 | 3.61 | 0 | identical | 30.53× |
| disk | api | q4 | 8 | 908.3 | 8.74 | 11.65 | 0 | identical | 79.16× |

### Gates: FAIL (exact) (273 checked, 1 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| **FAIL** | exact.phase | rocksdb/conc | – | – | – | – | the baseline ran the concurrency phase; the result did not |
| pass | time.query | snapshot/q1 | 1.30 | 0.11 | 0.64 | 3.00 |  |
| pass | time.query | snapshot/q2 | 50.80 | 39.24 | 0.78 | 3.00 |  |
| pass | time.query | snapshot/q3 | 2.19 | 0.41 | 0.57 | 3.00 |  |
| pass | time.query | snapshot/q4 | 53.63 | 0.10 | 0.04 | 3.00 |  |
| pass | time.query | snapshot/q5 | 333.25 | 205.34 | 0.62 | 3.00 |  |
| pass | time.query | snapshot/q6 | 187.61 | 125.92 | 0.67 | 3.00 |  |
| pass | time.query | snapshot/q7 | 402.96 | 271.40 | 0.68 | 3.00 |  |
| pass | time.query | snapshot/q8 | 363.48 | 224.72 | 0.62 | 3.00 |  |
| pass | time.query | snapshot/q9 | 529.28 | 317.18 | 0.60 | 3.00 |  |
| pass | time.query | snapshot/q10 | 326.63 | 200.25 | 0.62 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.09 | 1.12 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.59 | 0.50 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q13 | 223.92 | 144.58 | 0.65 | 3.00 |  |
| pass | time.query | snapshot/q14 | 132.13 | 90.70 | 0.69 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.33 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | snapshot | 76815.05 | 29761.62 | 0.39 | 2.00 |  |
| pass | time.open | snapshot | 1464.59 | 1047.47 | 0.72 | 2.50 |  |
| pass | time.query | disk/q1 | 39.06 | 4.30 | 0.15 | 3.00 |  |
| pass | time.query | disk/q2 | 189.65 | 51.98 | 0.28 | 3.00 |  |
| pass | time.query | disk/q3 | 39.17 | 4.71 | 0.16 | 3.00 |  |
| pass | time.query | disk/q4 | 75.19 | 2.16 | 0.05 | 3.00 |  |
| pass | time.query | disk/q5 | 621.34 | 269.33 | 0.44 | 3.00 |  |
| pass | time.query | disk/q6 | 511.38 | 166.72 | 0.33 | 3.00 |  |
| pass | time.query | disk/q7 | 1147.71 | 365.14 | 0.32 | 3.00 |  |
| pass | time.query | disk/q8 | 871.82 | 378.69 | 0.44 | 3.00 |  |
| pass | time.query | disk/q9 | 1450.56 | 499.97 | 0.35 | 3.00 |  |
| pass | time.query | disk/q10 | 697.13 | 283.85 | 0.41 | 3.00 |  |
| pass | time.query | disk/q11 | 1.49 | 0.98 | 0.85 | 3.00 |  |
| pass | time.query | disk/q12 | 0.72 | 0.56 | 0.94 | 3.00 |  |
| pass | time.query | disk/q13 | 906.09 | 242.57 | 0.27 | 3.00 |  |
| pass | time.query | disk/q14 | 320.35 | 121.03 | 0.38 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.25 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk | 1627585.07 | 21402.71 | 0.01 | 2.50 |  |
| pass | time.open | disk | 7673.50 | 1433.38 | 0.19 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 60.72 | 9.11 | 0.18 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 729.49 | 569.41 | 0.78 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 195.54 | 29.69 | 0.16 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 989.95 | 1.48 | 0.00 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 537.67 | 342.98 | 0.64 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 468.29 | 230.95 | 0.50 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 977.60 | 366.35 | 0.38 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 904.88 | 389.54 | 0.43 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 2125.90 | 715.42 | 0.34 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 594.89 | 368.73 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 16.79 | 2.08 | 0.22 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 10.19 | 0.30 | 0.19 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 511.89 | 257.35 | 0.50 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 382.71 | 201.26 | 0.53 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.21 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 277762.61 | 21485.89 | 0.08 | 2.00 |  |
| pass | time.open | rocksdb | 59389.43 | 869.85 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 710.34 | 588.53 | 0.84 | 1.50 |  |
| pass | rss.build_peak | snapshot | 741.02 | 550.00 | 0.75 | 1.50 |  |
| pass | rss.after_open | snapshot | 596.36 | 488.97 | 0.83 | 1.50 |  |
| pass | rss.query_hwm | disk | 740.63 | 460.73 | 0.64 | 1.50 |  |
| pass | rss.build_peak | disk | 738.11 | 434.44 | 0.61 | 1.50 |  |
| pass | rss.after_open | disk | 543.97 | 350.69 | 0.66 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 663.27 | 440.21 | 0.68 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 634.20 | 401.55 | 0.65 | 1.50 |  |
| pass | rss.after_open | rocksdb | 522.04 | 276.59 | 0.56 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 889778182.00 | 653959173.00 | 0.73 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206253032.00 | 155868540.00 | 0.76 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 574.14 | 5139.55 | 8.95 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 585.42 | 25106.37 | 42.89 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 17.13 | 4404.40 | 257.16 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 13.41 | 23334.23 | 1740.06 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 596.45 | 2972.53 | 4.98 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 617.81 | 10444.90 | 16.91 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 17.80 | 2373.40 | 133.31 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 15.37 | 10506.16 | 683.73 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 23.33 | 195.64 | 8.39 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 22.48 | 444.25 | 19.76 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 12.60 | 381.48 | 30.28 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 11.69 | 920.72 | 78.78 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 23.30 | 200.75 | 8.62 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 22.18 | 416.70 | 18.79 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 11.64 | 355.51 | 30.53 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 11.47 | 908.26 | 79.16 | – | report-only |

