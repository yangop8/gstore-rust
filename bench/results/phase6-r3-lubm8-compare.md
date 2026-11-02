## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.78 2.24 1.81
- calibration score 56.34 ms (sort 42.62, hashmap 93.21, format 45.01, fsync 0.26)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.31 | 112.7 | 85.5 | 209.0 (cold) | 187.1 | 102.0 | 128.8 | 3.11 | 0.39× |
| disk | 4.49 | 98.7 | 130.2 | 288.9 (cold) | 259.1 | 88.6 | 119.2 | 5.49 | 0.29× |
| rocksdb | 4.21 | 93.4 | 31.3 | 255.2 (cold) | 184.3 | 68.3 | 102.6 | 10.56 | 0.24× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.23 | 0.03 | 0.11× | 2.63 | 0.36 | 0.85× | 2.48 | 1.89 | 0.20× |
| q2 | 25 | 7.74 | 6.03 | 0.75× | 10.24 | 8.36 | 0.22× | 8.04 | 5.80 | 0.07× |
| q3 | 10 | 0.16 | 0.07 | 0.17× | 1.31 | 0.41 | 0.85× | 7.44 | 6.02 | 0.21× |
| q4 | 9 | 0.10 | 0.07 | 0.01× | 0.50 | 0.18 | 0.02× | 0.59 | 0.47 | 0.00× |
| q5 | 576 | 33.42 | 24.74 | 0.66× | 40.45 | 32.72 | 0.28× | 50.20 | 38.84 | 0.56× |
| q6 | 80056 | 22.27 | 21.93 | 0.71× | 29.39 | 28.36 | 0.29× | 39.18 | 39.45 | 0.58× |
| q7 | 23 | 37.46 | 30.47 | 0.56× | 47.89 | 45.21 | 0.21× | 47.70 | 46.14 | 0.35× |
| q8 | 10778 | 32.40 | 34.11 | 0.67× | 42.37 | 43.08 | 0.26× | 94.21 | 93.06 | 0.57× |
| q9 | 1081 | 45.25 | 44.27 | 0.63× | 63.05 | 64.05 | 0.24× | 115.65 | 106.81 | 0.44× |
| q10 | 3 | 23.74 | 22.64 | 0.63× | 32.72 | 30.22 | 0.27× | 38.42 | 38.30 | 0.55× |
| q11 | 325 | 0.60 | 0.24 | 0.62× | 0.77 | 0.32 | 0.59× | 1.61 | 1.04 | 0.38× |
| q12 | 21 | 0.23 | 0.13 | 0.86× | 0.31 | 0.13 | 0.82× | 0.29 | 0.19 | 0.13× |
| q13 | 85670 | 24.14 | 24.45 | 0.69× | 38.01 | 38.05 | 0.22× | 42.59 | 42.35 | 0.55× |
| q14 | 60576 | 15.35 | 16.10 | 0.76× | 19.10 | 19.75 | 0.33× | 30.21 | 30.65 | 0.61× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.17 | 0.00 / 0.01 | 5330 | 58.8 | 33.7 | 294.0 | 163.4 | 168432 → 169456 | ok |
| disk | 1.42 / 2.44 | 1.01 / 1.20 | 5330 | 606.0 | 759.1 | 0.0 | 568.6 | 168432 → 169456 | ok |
| rocksdb | 0.22 / 0.54 | 0.17 / 0.28 | 5330 | 119.2 | 103.8 | 29.0 | 151.1 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 10173.2 | 0.10 | 0.14 | 0 | identical | 5.08× |
| snapshot | sparql | q1 | 8 | 55390.7 | 0.14 | 0.31 | 0 | identical | 26.38× |
| snapshot | sparql | q4 | 1 | 5038.1 | 0.20 | 0.29 | 0 | identical | 62.18× |
| snapshot | sparql | q4 | 8 | 27252.9 | 0.29 | 0.61 | 0 | identical | 357.02× |
| snapshot | api | q1 | 1 | 5034.7 | 0.19 | 0.33 | 0 | identical | 3.11× |
| snapshot | api | q1 | 8 | 14788.1 | 0.53 | 1.17 | 0 | identical | 6.45× |
| snapshot | api | q4 | 1 | 2852.5 | 0.31 | 0.49 | 0 | identical | 33.31× |
| snapshot | api | q4 | 8 | 11469.6 | 0.65 | 1.35 | 0 | identical | 149.88× |
| disk | sparql | q1 | 1 | 1923.5 | 0.52 | 0.72 | 0 | identical | 1.25× |
| disk | sparql | q1 | 8 | 4698.4 | 1.70 | 3.16 | 0 | identical | 3.24× |
| disk | sparql | q4 | 1 | 2629.4 | 0.38 | 0.56 | 0 | identical | 30.98× |
| disk | sparql | q4 | 8 | 8206.3 | 0.97 | 1.92 | 0 | identical | 103.50× |
| disk | api | q1 | 1 | 1485.3 | 0.66 | 0.89 | 0 | identical | 1.23× |
| disk | api | q1 | 8 | 4213.0 | 1.88 | 3.51 | 0 | identical | 2.88× |
| disk | api | q4 | 1 | 1652.3 | 0.56 | 0.79 | 0 | identical | 19.24× |
| disk | api | q4 | 8 | 6046.1 | 1.27 | 2.50 | 0 | identical | 79.10× |

### Gates: FAIL (exact) (273 checked, 1 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| **FAIL** | exact.phase | rocksdb/conc | – | – | – | – | the baseline ran the concurrency phase; the result did not |
| pass | time.query | snapshot/q1 | 0.31 | 0.04 | 0.88 | 3.00 |  |
| pass | time.query | snapshot/q2 | 8.04 | 7.21 | 0.92 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.44 | 0.09 | 0.86 | 3.00 |  |
| pass | time.query | snapshot/q4 | 9.50 | 0.09 | 0.18 | 3.00 |  |
| pass | time.query | snapshot/q5 | 37.32 | 29.61 | 0.80 | 3.00 |  |
| pass | time.query | snapshot/q6 | 31.02 | 26.24 | 0.86 | 3.00 |  |
| pass | time.query | snapshot/q7 | 54.04 | 36.47 | 0.69 | 3.00 |  |
| pass | time.query | snapshot/q8 | 51.02 | 40.82 | 0.81 | 3.00 |  |
| pass | time.query | snapshot/q9 | 70.37 | 52.98 | 0.76 | 3.00 |  |
| pass | time.query | snapshot/q10 | 36.11 | 27.10 | 0.76 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.39 | 0.29 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.16 | 0.16 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q13 | 35.21 | 29.27 | 0.84 | 3.00 |  |
| pass | time.query | snapshot/q14 | 21.10 | 19.27 | 0.92 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.51 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 15315.60 | 6353.37 | 0.42 | 2.00 |  |
| pass | time.open | snapshot | 292.10 | 223.92 | 0.78 | 2.50 |  |
| pass | time.query | disk/q1 | 0.42 | 0.42 | 1.00 | 3.00 |  |
| pass | time.query | disk/q2 | 38.39 | 9.81 | 0.29 | 3.00 |  |
| pass | time.query | disk/q3 | 0.48 | 0.48 | 1.00 | 3.00 |  |
| pass | time.query | disk/q4 | 9.73 | 0.21 | 0.19 | 3.00 |  |
| pass | time.query | disk/q5 | 115.47 | 38.42 | 0.34 | 3.00 |  |
| pass | time.query | disk/q6 | 97.27 | 33.30 | 0.36 | 3.00 |  |
| pass | time.query | disk/q7 | 216.50 | 53.08 | 0.25 | 3.00 |  |
| pass | time.query | disk/q8 | 164.92 | 50.58 | 0.32 | 3.00 |  |
| pass | time.query | disk/q9 | 266.26 | 75.20 | 0.29 | 3.00 |  |
| pass | time.query | disk/q10 | 114.01 | 35.48 | 0.32 | 3.00 |  |
| pass | time.query | disk/q11 | 0.54 | 0.38 | 0.93 | 3.00 |  |
| pass | time.query | disk/q12 | 0.15 | 0.15 | 1.00 | 3.00 |  |
| pass | time.query | disk/q13 | 172.05 | 44.67 | 0.27 | 3.00 |  |
| pass | time.query | disk/q14 | 60.17 | 23.18 | 0.41 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.23 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk | 263743.67 | 5275.44 | 0.02 | 2.50 |  |
| pass | time.open | disk | 1447.34 | 304.14 | 0.22 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.47 | 1.89 | 0.34 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 87.10 | 5.81 | 0.09 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 29.34 | 6.03 | 0.26 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 123.27 | 0.47 | 0.02 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 69.86 | 38.89 | 0.57 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 68.19 | 39.50 | 0.59 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 131.77 | 46.20 | 0.36 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 162.09 | 93.19 | 0.58 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 241.39 | 106.95 | 0.45 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 69.61 | 38.34 | 0.56 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.71 | 1.04 | 0.65 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.52 | 0.19 | 0.62 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 76.77 | 42.40 | 0.56 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 49.97 | 30.69 | 0.63 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.24 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 53931.02 | 4217.59 | 0.08 | 2.00 |  |
| pass | time.open | rocksdb | 9988.71 | 184.59 | 0.02 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 152.54 | 128.79 | 0.87 | 1.50 |  |
| pass | rss.build_peak | snapshot | 154.20 | 112.67 | 0.78 | 1.50 |  |
| pass | rss.after_open | snapshot | 125.84 | 101.97 | 0.85 | 1.50 |  |
| pass | rss.query_hwm | disk | 167.48 | 119.25 | 0.76 | 1.50 |  |
| pass | rss.build_peak | disk | 164.71 | 98.72 | 0.66 | 1.50 |  |
| pass | rss.after_open | disk | 129.73 | 88.63 | 0.75 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 184.86 | 102.61 | 0.62 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 285.53 | 93.36 | 0.39 | 1.50 |  |
| pass | rss.after_open | rocksdb | 145.78 | 68.27 | 0.56 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 175005702.00 | 130150405.00 | 0.74 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41162410.00 | 31266467.00 | 0.76 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 2000.98 | 10173.24 | 5.08 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 2099.42 | 55390.72 | 26.38 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 81.03 | 5038.13 | 62.18 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 76.33 | 27252.85 | 357.02 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 1616.66 | 5034.70 | 3.11 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 2293.12 | 14788.13 | 6.45 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 85.62 | 2852.46 | 33.31 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 76.53 | 11469.60 | 149.88 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1542.54 | 1923.50 | 1.25 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1449.55 | 4698.40 | 3.24 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 84.87 | 2629.36 | 30.98 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 79.29 | 8206.31 | 103.50 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1211.81 | 1485.28 | 1.23 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1460.59 | 4212.98 | 2.88 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 85.88 | 1652.34 | 19.24 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 76.44 | 6046.12 | 79.10 | – | report-only |

