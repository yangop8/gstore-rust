## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 1.20 1.41 1.51
- calibration score 60.65 ms (sort 46.57, hashmap 104.98, format 45.63, fsync 0.46)
- compared with: reference profile, commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 28.84 | 550.0 | 430.8 | 1042.9 (cold) | 1015.1 | 489.0 | 588.5 | 16.14 | 0.88× |
| disk | 21.01 | 434.4 | 654.0 | 1491.2 (cold) | 1407.2 | 350.7 | 460.7 | 39.77 | 1.06× |
| rocksdb | 22.25 | 401.6 | 155.9 | 1005.8 (cold) | 900.7 | 276.6 | 440.2 | 66.91 | 0.25× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.36 | 0.10 | 0.58× | 7.78 | 4.22 | 0.97× | 11.52 | 9.43 | 0.15× |
| q2 | 115 | 40.48 | 38.03 | 0.96× | 51.41 | 51.03 | 1.04× | 550.84 | 589.62 | 0.51× |
| q3 | 10 | 0.59 | 0.40 | 0.65× | 7.16 | 4.63 | 1.00× | 36.08 | 30.74 | 0.17× |
| q4 | 9 | 0.15 | 0.10 | 0.86× | 2.70 | 2.12 | 1.02× | 1.94 | 1.53 | 0.00× |
| q5 | 576 | 233.18 | 199.00 | 0.87× | 317.55 | 264.42 | 1.09× | 432.71 | 355.15 | 0.76× |
| q6 | 404399 | 119.18 | 122.03 | 0.98× | 157.30 | 163.68 | 1.07× | 231.35 | 239.14 | 0.69× |
| q7 | 23 | 259.70 | 263.02 | 0.94× | 376.97 | 358.48 | 1.04× | 381.96 | 379.34 | 0.50× |
| q8 | 10778 | 226.49 | 217.78 | 0.93× | 318.06 | 371.78 | 1.24× | 391.19 | 403.36 | 0.55× |
| q9 | 5507 | 305.09 | 307.39 | 0.91× | 435.78 | 490.84 | 1.08× | 741.45 | 740.81 | 0.39× |
| q10 | 3 | 191.32 | 194.07 | 0.85× | 327.43 | 278.67 | 1.13× | 337.99 | 381.82 | 0.87× |
| q11 | 325 | 2.26 | 1.08 | 0.96× | 2.50 | 0.96 | 0.96× | 8.90 | 2.15 | 0.13× |
| q12 | 21 | 1.07 | 0.48 | 1.04× | 1.90 | 0.55 | 1.03× | 0.44 | 0.31 | 0.03× |
| q13 | 432512 | 138.65 | 140.12 | 0.98× | 229.10 | 238.14 | 1.11× | 257.71 | 266.48 | 0.77× |
| q14 | 305503 | 84.81 | 87.90 | 0.98× | 110.19 | 118.82 | 1.09× | 197.42 | 208.40 | 0.79× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.02 / 0.20 | 0.00 / 0.01 | 6478 | 54.9 | 29.9 | 1345.7 | 835.2 | 842657 → 843783 | ok |
| disk | 1.35 / 2.35 | 1.30 / 1.98 | 6478 | 1100.1 | 1042.0 | 0.0 | 2816.7 | 842657 → 843783 | ok |
| rocksdb | 0.22 / 0.37 | 0.21 / 0.34 | 6478 | 153.3 | 191.7 | 37.2 | 730.3 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 5139.5 | 0.19 | 0.32 | 0 | identical | 1.75× |
| snapshot | sparql | q1 | 8 | 25106.4 | 0.31 | 0.65 | 0 | identical | 8.75× |
| snapshot | sparql | q4 | 1 | 4404.4 | 0.22 | 0.36 | 0 | identical | 1.18× |
| snapshot | sparql | q4 | 8 | 23334.2 | 0.34 | 0.70 | 0 | identical | 5.01× |
| snapshot | api | q1 | 1 | 2972.5 | 0.32 | 0.51 | 0 | identical | 1.44× |
| snapshot | api | q1 | 8 | 10444.9 | 0.75 | 1.52 | 0 | identical | 3.41× |
| snapshot | api | q4 | 1 | 2373.4 | 0.38 | 0.59 | 0 | identical | 1.19× |
| snapshot | api | q4 | 8 | 10506.2 | 0.71 | 1.49 | 0 | identical | 2.31× |
| disk | sparql | q1 | 1 | 195.6 | 5.11 | 6.87 | 0 | identical | 1.07× |
| disk | sparql | q1 | 8 | 444.2 | 18.00 | 26.93 | 0 | identical | 2.95× |
| disk | sparql | q4 | 1 | 381.5 | 2.62 | 3.46 | 0 | identical | 1.13× |
| disk | sparql | q4 | 8 | 920.7 | 8.68 | 11.39 | 0 | identical | 3.32× |
| disk | api | q1 | 1 | 200.7 | 4.96 | 6.46 | 0 | identical | 1.16× |
| disk | api | q1 | 8 | 416.7 | 19.14 | 30.65 | 0 | identical | 3.08× |
| disk | api | q4 | 1 | 355.5 | 2.76 | 3.61 | 0 | identical | 1.15× |
| disk | api | q4 | 8 | 908.3 | 8.74 | 11.65 | 0 | identical | 3.97× |

### Gates: FAIL (exact) (273 checked, 1 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| **FAIL** | exact.phase | rocksdb/conc | – | – | – | – | the baseline ran the concurrency phase; the result did not |
| pass | time.query | snapshot/q1 | 0.18 | 0.11 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q2 | 39.60 | 38.52 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.61 | 0.40 | 0.92 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.11 | 0.10 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q5 | 228.92 | 201.58 | 0.88 | 3.00 |  |
| pass | time.query | snapshot/q6 | 124.71 | 123.61 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q7 | 279.15 | 266.43 | 0.95 | 3.00 |  |
| pass | time.query | snapshot/q8 | 234.27 | 220.60 | 0.94 | 3.00 |  |
| pass | time.query | snapshot/q9 | 336.80 | 311.37 | 0.92 | 3.00 |  |
| pass | time.query | snapshot/q10 | 229.32 | 196.59 | 0.86 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.12 | 1.10 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.46 | 0.49 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q13 | 142.92 | 141.93 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q14 | 89.57 | 89.04 | 0.99 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.95 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 28318.38 | 29216.65 | 1.03 | 2.00 |  |
| pass | time.open | snapshot | 898.03 | 1028.29 | 1.14 | 2.50 |  |
| pass | time.query | disk/q1 | 4.35 | 4.22 | 0.98 | 3.00 |  |
| pass | time.query | disk/q2 | 48.86 | 51.03 | 1.04 | 3.00 |  |
| pass | time.query | disk/q3 | 4.61 | 4.63 | 1.00 | 3.00 |  |
| pass | time.query | disk/q4 | 2.09 | 2.12 | 1.01 | 3.00 |  |
| pass | time.query | disk/q5 | 242.15 | 264.40 | 1.09 | 3.00 |  |
| pass | time.query | disk/q6 | 153.32 | 163.67 | 1.07 | 3.00 |  |
| pass | time.query | disk/q7 | 344.47 | 358.46 | 1.04 | 3.00 |  |
| pass | time.query | disk/q8 | 299.37 | 371.76 | 1.24 | 3.00 |  |
| pass | time.query | disk/q9 | 453.39 | 490.81 | 1.08 | 3.00 |  |
| pass | time.query | disk/q10 | 245.81 | 278.65 | 1.13 | 3.00 |  |
| pass | time.query | disk/q11 | 1.00 | 0.96 | 0.99 | 3.00 |  |
| pass | time.query | disk/q12 | 0.53 | 0.55 | 1.01 | 3.00 |  |
| pass | time.query | disk/q13 | 215.47 | 238.12 | 1.10 | 3.00 |  |
| pass | time.query | disk/q14 | 109.37 | 118.82 | 1.08 | 3.00 |  |
| pass | time.geomean | disk | – | – | 1.07 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | disk | 19844.28 | 21010.80 | 1.06 | 2.50 |  |
| pass | time.open | disk | 1582.00 | 1407.13 | 0.89 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 62.72 | 10.89 | 0.20 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 1152.14 | 681.25 | 0.59 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 185.81 | 35.52 | 0.20 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 438.05 | 1.77 | 0.01 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 465.94 | 410.35 | 0.88 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 346.18 | 276.31 | 0.80 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 763.28 | 438.30 | 0.58 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 733.13 | 466.05 | 0.64 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 1900.32 | 855.94 | 0.45 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 436.99 | 441.16 | 1.01 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 16.99 | 2.48 | 0.24 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 10.85 | 0.36 | 0.18 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 347.93 | 307.90 | 0.89 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 264.89 | 240.79 | 0.91 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.29 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 164351.21 | 25705.93 | 0.16 | 2.00 |  |
| pass | time.open | rocksdb | 937.41 | 1040.70 | 1.11 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 590.36 | 588.53 | 1.00 | 1.50 |  |
| pass | rss.build_peak | snapshot | 549.84 | 550.00 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 488.62 | 488.97 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk | 460.23 | 460.73 | 1.00 | 1.50 |  |
| pass | rss.build_peak | disk | 400.41 | 434.44 | 1.08 | 1.50 |  |
| pass | rss.after_open | disk | 350.09 | 350.69 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 386.38 | 440.21 | 1.13 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 510.58 | 401.55 | 0.80 | 1.50 |  |
| pass | rss.after_open | rocksdb | 199.84 | 276.59 | 1.33 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 653959174.00 | 653959173.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 206221265.00 | 155868540.00 | 0.76 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 2941.93 | 5139.55 | 1.75 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 2868.76 | 25106.37 | 8.75 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 3733.98 | 4404.40 | 1.18 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 4657.96 | 23334.23 | 5.01 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 2068.46 | 2972.53 | 1.44 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 3065.87 | 10444.90 | 3.41 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 1989.42 | 2373.40 | 1.19 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 4556.24 | 10506.16 | 2.31 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 183.45 | 195.64 | 1.07 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 150.47 | 444.25 | 2.95 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 338.24 | 381.48 | 1.13 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 277.41 | 920.72 | 3.32 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 173.67 | 200.75 | 1.16 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 135.22 | 416.70 | 3.08 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 308.55 | 355.51 | 1.15 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 228.93 | 908.26 | 3.97 | – | report-only |

