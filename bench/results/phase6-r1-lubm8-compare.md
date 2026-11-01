## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `05be7e3d58b27605c7e1eb2680039e52fb961743`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.82 4.03 3.61
- calibration score 60.71 ms (sort 43.47, hashmap 106.49, format 48.33, fsync 0.43)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.33 | 138.1 | 85.5 | 299.8 (cold) | 243.3 | 128.1 | 154.0 | 3.06 | 0.39× |
| disk | 90.14 | 123.1 | 175.0 | 1123.7 (cold) | 317.5 | 114.5 | 145.2 | 5.63 | 0.30× |
| rocksdb | 31.49 | 286.8 | 41.1 | 305.7 (cold) | 291.6 | 90.2 | 138.8 | 31.10 | 0.72× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.23 | 0.04 | 0.14× | 2.75 | 0.36 | 0.86× | 9.39 | 8.61 | 0.91× |
| q2 | 25 | 7.60 | 5.89 | 0.73× | 10.40 | 8.60 | 0.22× | 38.99 | 33.47 | 0.38× |
| q3 | 10 | 0.18 | 0.09 | 0.22× | 1.46 | 0.43 | 0.89× | 29.15 | 28.06 | 0.96× |
| q4 | 9 | 0.10 | 0.06 | 0.01× | 0.50 | 0.18 | 0.02× | 68.35 | 57.54 | 0.47× |
| q5 | 576 | 32.53 | 23.06 | 0.62× | 40.21 | 31.61 | 0.27× | 55.64 | 49.56 | 0.71× |
| q6 | 80056 | 20.38 | 19.92 | 0.64× | 28.71 | 28.36 | 0.29× | 44.59 | 45.91 | 0.67× |
| q7 | 23 | 32.37 | 30.69 | 0.57× | 50.70 | 49.72 | 0.23× | 108.44 | 103.41 | 0.78× |
| q8 | 10778 | 30.69 | 32.04 | 0.63× | 46.55 | 47.36 | 0.29× | 171.72 | 126.00 | 0.78× |
| q9 | 1081 | 43.11 | 42.72 | 0.61× | 67.13 | 65.40 | 0.25× | 237.71 | 212.45 | 0.88× |
| q10 | 3 | 21.97 | 22.06 | 0.61× | 34.47 | 33.62 | 0.29× | 51.32 | 50.89 | 0.73× |
| q11 | 325 | 0.58 | 0.24 | 0.62× | 0.82 | 0.33 | 0.60× | 2.73 | 2.30 | 0.85× |
| q12 | 21 | 0.22 | 0.14 | 0.88× | 0.35 | 0.13 | 0.81× | 1.43 | 1.22 | 0.80× |
| q13 | 85670 | 22.35 | 22.21 | 0.63× | 39.06 | 37.90 | 0.22× | 52.30 | 50.33 | 0.66× |
| q14 | 60576 | 14.62 | 14.30 | 0.68× | 19.32 | 19.11 | 0.32× | 37.16 | 37.77 | 0.76× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.16 | 0.00 / 0.01 | 5330 | 44.6 | 25.1 | 303.5 | 239.1 | 168432 → 169456 | ok |
| disk | 1.10 / 1.60 | 0.93 / 1.28 | 5330 | 823.2 | 812.4 | 0.0 | 678.6 | 168432 → 169456 | ok |
| rocksdb | 0.22 / 0.38 | 0.22 / 0.28 | 5330 | 125.6 | 101.2 | 27.2 | 166.4 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 9336.1 | 0.11 | 0.15 | 0 | identical | 4.67× |
| snapshot | sparql | q1 | 8 | 14209.9 | 0.56 | 1.29 | 0 | identical | 6.77× |
| snapshot | sparql | q4 | 1 | 5309.5 | 0.19 | 0.28 | 0 | identical | 65.53× |
| snapshot | sparql | q4 | 8 | 7180.4 | 1.11 | 2.21 | 0 | identical | 94.07× |
| snapshot | api | q1 | 1 | 4463.9 | 0.21 | 0.35 | 0 | identical | 2.76× |
| snapshot | api | q1 | 8 | 9418.4 | 0.83 | 1.67 | 0 | identical | 4.11× |
| snapshot | api | q4 | 1 | 2176.9 | 0.41 | 0.59 | 0 | identical | 25.42× |
| snapshot | api | q4 | 8 | 5697.4 | 1.34 | 2.64 | 0 | identical | 74.45× |
| disk | sparql | q1 | 1 | 1851.2 | 0.54 | 0.74 | 0 | identical | 1.20× |
| disk | sparql | q1 | 8 | 1739.1 | 4.60 | 5.37 | 0 | identical | 1.20× |
| disk | sparql | q4 | 1 | 2470.8 | 0.40 | 0.56 | 0 | identical | 29.11× |
| disk | sparql | q4 | 8 | 2439.2 | 3.28 | 3.86 | 0 | identical | 30.76× |
| disk | api | q1 | 1 | 1296.0 | 0.76 | 0.99 | 0 | identical | 1.07× |
| disk | api | q1 | 8 | 1675.8 | 4.75 | 5.48 | 0 | identical | 1.15× |
| disk | api | q4 | 1 | 1472.9 | 0.63 | 0.85 | 0 | identical | 17.15× |
| disk | api | q4 | 8 | 2392.3 | 3.28 | 6.16 | 0 | identical | 31.30× |
| rocksdb | sparql | q1 | 1 | 87.2 | 11.46 | 16.28 | 0 | identical | 1.09× |
| rocksdb | sparql | q1 | 8 | 78.3 | 101.23 | 121.12 | 0 | identical | 1.04× |
| rocksdb | sparql | q4 | 1 | 14.9 | 67.14 | 95.81 | 0 | identical | 2.08× |
| rocksdb | sparql | q4 | 8 | 13.7 | 555.01 | 644.98 | 0 | identical | 2.18× |
| rocksdb | api | q1 | 1 | 100.3 | 9.95 | 15.29 | 0 | identical | 1.07× |
| rocksdb | api | q1 | 8 | 91.0 | 87.18 | 104.11 | 0 | identical | 1.17× |
| rocksdb | api | q4 | 1 | 15.2 | 65.72 | 99.12 | 0 | identical | 2.04× |
| rocksdb | api | q4 | 8 | 12.3 | 622.16 | 920.82 | 0 | identical | 1.85× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.31 | 0.05 | 0.89 | 3.00 |  |
| pass | time.query | snapshot/q2 | 8.04 | 6.54 | 0.85 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.44 | 0.11 | 0.86 | 3.00 |  |
| pass | time.query | snapshot/q4 | 9.50 | 0.07 | 0.18 | 3.00 |  |
| pass | time.query | snapshot/q5 | 37.32 | 25.61 | 0.70 | 3.00 |  |
| pass | time.query | snapshot/q6 | 31.02 | 22.12 | 0.73 | 3.00 |  |
| pass | time.query | snapshot/q7 | 54.04 | 34.08 | 0.64 | 3.00 |  |
| pass | time.query | snapshot/q8 | 51.02 | 35.59 | 0.71 | 3.00 |  |
| pass | time.query | snapshot/q9 | 70.37 | 47.44 | 0.68 | 3.00 |  |
| pass | time.query | snapshot/q10 | 36.11 | 24.51 | 0.70 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.39 | 0.27 | 0.95 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.16 | 0.15 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q13 | 35.21 | 24.66 | 0.72 | 3.00 |  |
| pass | time.query | snapshot/q14 | 21.10 | 15.88 | 0.77 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.44 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 15315.60 | 5924.46 | 0.39 | 2.00 |  |
| pass | time.open | snapshot | 292.10 | 270.21 | 0.93 | 2.50 |  |
| pass | time.query | disk/q1 | 0.42 | 0.40 | 0.99 | 3.00 |  |
| pass | time.query | disk/q2 | 38.39 | 9.55 | 0.29 | 3.00 |  |
| pass | time.query | disk/q3 | 0.48 | 0.47 | 1.00 | 3.00 |  |
| pass | time.query | disk/q4 | 9.73 | 0.19 | 0.19 | 3.00 |  |
| pass | time.query | disk/q5 | 115.47 | 35.11 | 0.32 | 3.00 |  |
| pass | time.query | disk/q6 | 97.27 | 31.49 | 0.34 | 3.00 |  |
| pass | time.query | disk/q7 | 216.50 | 55.22 | 0.26 | 3.00 |  |
| pass | time.query | disk/q8 | 164.92 | 52.60 | 0.33 | 3.00 |  |
| pass | time.query | disk/q9 | 266.26 | 72.64 | 0.28 | 3.00 |  |
| pass | time.query | disk/q10 | 114.01 | 37.34 | 0.34 | 3.00 |  |
| pass | time.query | disk/q11 | 0.54 | 0.36 | 0.93 | 3.00 |  |
| pass | time.query | disk/q12 | 0.15 | 0.14 | 0.99 | 3.00 |  |
| pass | time.query | disk/q13 | 172.05 | 42.09 | 0.25 | 3.00 |  |
| pass | time.query | disk/q14 | 60.17 | 21.22 | 0.37 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.22 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk | 263743.67 | 100110.45 | 0.38 | 2.50 |  |
| pass | time.open | disk | 1447.34 | 352.65 | 0.25 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.47 | 8.76 | 0.94 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 87.10 | 34.04 | 0.40 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 29.34 | 28.54 | 0.97 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 123.27 | 58.52 | 0.48 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 69.86 | 50.41 | 0.73 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 68.19 | 46.70 | 0.69 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 131.77 | 105.17 | 0.80 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 162.09 | 128.14 | 0.79 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 241.39 | 216.06 | 0.90 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 69.61 | 51.75 | 0.75 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.71 | 2.34 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.52 | 1.24 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 76.77 | 51.18 | 0.68 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 49.97 | 38.41 | 0.78 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.73 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 53931.02 | 32030.16 | 0.59 | 2.00 |  |
| pass | time.open | rocksdb | 9988.71 | 296.60 | 0.03 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 152.54 | 154.04 | 1.01 | 1.50 |  |
| pass | rss.build_peak | snapshot | 154.20 | 138.11 | 0.91 | 1.50 |  |
| pass | rss.after_open | snapshot | 125.84 | 128.07 | 1.01 | 1.50 |  |
| pass | rss.query_hwm | disk | 167.48 | 145.19 | 0.89 | 1.50 |  |
| pass | rss.build_peak | disk | 164.71 | 123.09 | 0.79 | 1.50 |  |
| pass | rss.after_open | disk | 129.73 | 114.50 | 0.91 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 184.86 | 138.76 | 0.79 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 285.53 | 286.77 | 1.00 | 1.50 |  |
| pass | rss.after_open | rocksdb | 145.78 | 90.22 | 0.69 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 175005702.00 | 175005701.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41162410.00 | 41135741.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 2000.98 | 9336.07 | 4.67 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 2099.42 | 14209.92 | 6.77 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 81.03 | 5309.53 | 65.53 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 76.33 | 7180.39 | 94.07 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 1616.66 | 4463.93 | 2.76 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 2293.12 | 9418.45 | 4.11 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 85.62 | 2176.91 | 25.42 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 76.53 | 5697.36 | 74.45 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1542.54 | 1851.17 | 1.20 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1449.55 | 1739.10 | 1.20 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 84.87 | 2470.85 | 29.11 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 79.29 | 2439.24 | 30.76 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1211.81 | 1295.95 | 1.07 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1460.59 | 1675.79 | 1.15 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 85.88 | 1472.86 | 17.15 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 76.44 | 2392.26 | 31.30 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 80.25 | 87.23 | 1.09 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 75.32 | 78.30 | 1.04 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 7.17 | 14.89 | 2.08 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 6.30 | 13.75 | 2.18 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 93.56 | 100.28 | 1.07 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 77.89 | 90.98 | 1.17 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 7.45 | 15.20 | 2.04 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 6.68 | 12.34 | 1.85 | – | report-only |

