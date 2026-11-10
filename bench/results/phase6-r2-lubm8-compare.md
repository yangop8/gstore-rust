## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.24 2.52 3.31
- calibration score 63.35 ms (sort 45.24, hashmap 110.51, format 50.85, fsync 0.33)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.80 | 112.4 | 85.5 | 213.4 (cold) | 176.8 | 101.7 | 126.8 | 3.29 | 0.42× |
| disk | 4.04 | 96.9 | 130.2 | 294.5 (cold) | 258.8 | 88.2 | 118.3 | 5.87 | 0.31× |
| rocksdb | 35.60 | 261.5 | 41.1 | 202.3 (cold) | 180.4 | 64.5 | 112.5 | 33.28 | 0.77× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.24 | 0.05 | 0.15× | 2.65 | 0.38 | 0.91× | 11.17 | 9.72 | 1.03× |
| q2 | 25 | 7.78 | 6.08 | 0.76× | 10.07 | 8.37 | 0.22× | 40.64 | 36.01 | 0.41× |
| q3 | 10 | 0.20 | 0.12 | 0.27× | 1.39 | 0.47 | 0.98× | 30.91 | 29.44 | 1.00× |
| q4 | 9 | 0.10 | 0.06 | 0.01× | 0.54 | 0.18 | 0.02× | 70.04 | 59.02 | 0.48× |
| q5 | 576 | 34.51 | 24.69 | 0.66× | 41.66 | 33.23 | 0.29× | 56.47 | 51.83 | 0.74× |
| q6 | 80056 | 21.98 | 21.07 | 0.68× | 30.43 | 30.43 | 0.31× | 50.89 | 50.70 | 0.74× |
| q7 | 23 | 32.02 | 30.89 | 0.57× | 52.79 | 53.01 | 0.24× | 112.25 | 103.85 | 0.79× |
| q8 | 10778 | 34.94 | 35.68 | 0.70× | 48.96 | 51.07 | 0.31× | 137.72 | 135.20 | 0.83× |
| q9 | 1081 | 47.18 | 45.82 | 0.65× | 67.61 | 72.68 | 0.27× | 233.61 | 217.16 | 0.90× |
| q10 | 3 | 23.21 | 22.64 | 0.63× | 34.81 | 32.74 | 0.29× | 53.89 | 54.62 | 0.78× |
| q11 | 325 | 0.60 | 0.25 | 0.64× | 0.86 | 0.33 | 0.60× | 3.25 | 2.46 | 0.91× |
| q12 | 21 | 0.23 | 0.13 | 0.85× | 0.35 | 0.12 | 0.80× | 1.72 | 1.37 | 0.90× |
| q13 | 85670 | 24.23 | 24.83 | 0.71× | 39.68 | 41.35 | 0.24× | 53.98 | 55.15 | 0.72× |
| q14 | 60576 | 16.35 | 16.55 | 0.78× | 20.67 | 20.12 | 0.33× | 41.47 | 41.23 | 0.83× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.22 | 0.01 / 0.01 | 5330 | 52.5 | 29.4 | 319.0 | 181.8 | 168432 → 169456 | ok |
| disk | 1.69 / 2.50 | 1.36 / 2.37 | 5330 | 835.8 | 877.9 | 0.1 | 637.4 | 168432 → 169456 | ok |
| rocksdb | 0.25 / 0.36 | 0.21 / 0.27 | 5330 | 156.1 | 106.1 | 27.4 | 152.9 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 7867.4 | 0.13 | 0.18 | 0 | identical | 3.93× |
| snapshot | sparql | q1 | 8 | 11487.6 | 0.70 | 1.40 | 0 | identical | 5.47× |
| snapshot | sparql | q4 | 1 | 5237.7 | 0.19 | 0.32 | 0 | identical | 64.64× |
| snapshot | sparql | q4 | 8 | 6009.7 | 1.33 | 2.67 | 0 | identical | 78.73× |
| snapshot | api | q1 | 1 | 3229.7 | 0.30 | 0.49 | 0 | identical | 2.00× |
| snapshot | api | q1 | 8 | 8368.6 | 0.94 | 1.89 | 0 | identical | 3.65× |
| snapshot | api | q4 | 1 | 2223.1 | 0.40 | 0.61 | 0 | identical | 25.96× |
| snapshot | api | q4 | 8 | 5707.7 | 1.34 | 2.68 | 0 | identical | 74.58× |
| disk | sparql | q1 | 1 | 1586.4 | 0.63 | 0.83 | 0 | identical | 1.03× |
| disk | sparql | q1 | 8 | 1694.8 | 4.72 | 5.94 | 0 | identical | 1.17× |
| disk | sparql | q4 | 1 | 2362.3 | 0.42 | 0.59 | 0 | identical | 27.83× |
| disk | sparql | q4 | 8 | 2436.6 | 3.28 | 4.02 | 0 | identical | 30.73× |
| disk | api | q1 | 1 | 1324.9 | 0.74 | 1.01 | 0 | identical | 1.09× |
| disk | api | q1 | 8 | 1751.6 | 4.55 | 5.78 | 0 | identical | 1.20× |
| disk | api | q4 | 1 | 1382.6 | 0.67 | 1.04 | 0 | identical | 16.10× |
| disk | api | q4 | 8 | 2321.1 | 3.38 | 6.30 | 0 | identical | 30.37× |
| rocksdb | sparql | q1 | 1 | 98.3 | 10.17 | 16.19 | 0 | identical | 1.23× |
| rocksdb | sparql | q1 | 8 | 86.0 | 92.27 | 116.22 | 0 | identical | 1.14× |
| rocksdb | sparql | q4 | 1 | 14.9 | 67.12 | 100.98 | 0 | identical | 2.08× |
| rocksdb | sparql | q4 | 8 | 13.6 | 558.29 | 672.33 | 0 | identical | 2.17× |
| rocksdb | api | q1 | 1 | 93.8 | 10.64 | 16.12 | 0 | identical | 1.00× |
| rocksdb | api | q1 | 8 | 80.9 | 98.01 | 119.99 | 0 | identical | 1.04× |
| rocksdb | api | q4 | 1 | 14.6 | 68.57 | 99.88 | 0 | identical | 1.95× |
| rocksdb | api | q4 | 8 | 13.5 | 565.99 | 692.26 | 0 | identical | 2.03× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.31 | 0.05 | 0.89 | 3.00 |  |
| pass | time.query | snapshot/q2 | 8.04 | 6.47 | 0.84 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.44 | 0.12 | 0.87 | 3.00 |  |
| pass | time.query | snapshot/q4 | 9.50 | 0.07 | 0.18 | 3.00 |  |
| pass | time.query | snapshot/q5 | 37.32 | 26.28 | 0.72 | 3.00 |  |
| pass | time.query | snapshot/q6 | 31.02 | 22.43 | 0.74 | 3.00 |  |
| pass | time.query | snapshot/q7 | 54.04 | 32.88 | 0.62 | 3.00 |  |
| pass | time.query | snapshot/q8 | 51.02 | 37.98 | 0.75 | 3.00 |  |
| pass | time.query | snapshot/q9 | 70.37 | 48.76 | 0.70 | 3.00 |  |
| pass | time.query | snapshot/q10 | 36.11 | 24.10 | 0.68 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.39 | 0.27 | 0.95 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.16 | 0.14 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q13 | 35.21 | 26.43 | 0.76 | 3.00 |  |
| pass | time.query | snapshot/q14 | 21.10 | 17.61 | 0.85 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.46 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 15315.60 | 6169.59 | 0.41 | 2.00 |  |
| pass | time.open | snapshot | 292.10 | 188.18 | 0.67 | 2.50 |  |
| pass | time.query | disk/q1 | 0.42 | 0.41 | 0.99 | 3.00 |  |
| pass | time.query | disk/q2 | 38.39 | 8.91 | 0.27 | 3.00 |  |
| pass | time.query | disk/q3 | 0.48 | 0.50 | 1.01 | 3.00 |  |
| pass | time.query | disk/q4 | 9.73 | 0.19 | 0.19 | 3.00 |  |
| pass | time.query | disk/q5 | 115.47 | 35.37 | 0.32 | 3.00 |  |
| pass | time.query | disk/q6 | 97.27 | 32.39 | 0.35 | 3.00 |  |
| pass | time.query | disk/q7 | 216.50 | 56.42 | 0.27 | 3.00 |  |
| pass | time.query | disk/q8 | 164.92 | 54.35 | 0.34 | 3.00 |  |
| pass | time.query | disk/q9 | 266.26 | 77.35 | 0.30 | 3.00 |  |
| pass | time.query | disk/q10 | 114.01 | 34.84 | 0.32 | 3.00 |  |
| pass | time.query | disk/q11 | 0.54 | 0.35 | 0.92 | 3.00 |  |
| pass | time.query | disk/q12 | 0.15 | 0.13 | 0.99 | 3.00 |  |
| pass | time.query | disk/q13 | 172.05 | 44.02 | 0.26 | 3.00 |  |
| pass | time.query | disk/q14 | 60.17 | 21.42 | 0.38 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.22 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk | 263743.67 | 4295.24 | 0.02 | 2.50 |  |
| pass | time.open | disk | 1447.34 | 275.48 | 0.20 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.47 | 9.61 | 1.01 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 87.10 | 35.62 | 0.42 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 29.34 | 29.12 | 0.99 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 123.27 | 58.38 | 0.48 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 69.86 | 51.27 | 0.74 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 68.19 | 50.15 | 0.74 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 131.77 | 102.72 | 0.78 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 162.09 | 133.73 | 0.83 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 241.39 | 214.79 | 0.89 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 69.61 | 54.02 | 0.78 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.71 | 2.43 | 0.94 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.52 | 1.36 | 0.95 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 76.77 | 54.55 | 0.72 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 49.97 | 40.78 | 0.82 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.76 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 53931.02 | 35212.08 | 0.65 | 2.00 |  |
| pass | time.open | rocksdb | 9988.71 | 178.48 | 0.02 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 152.54 | 126.75 | 0.86 | 1.50 |  |
| pass | rss.build_peak | snapshot | 154.20 | 112.43 | 0.78 | 1.50 |  |
| pass | rss.after_open | snapshot | 125.84 | 101.67 | 0.85 | 1.50 |  |
| pass | rss.query_hwm | disk | 167.48 | 118.30 | 0.75 | 1.50 |  |
| pass | rss.build_peak | disk | 164.71 | 96.86 | 0.66 | 1.50 |  |
| pass | rss.after_open | disk | 129.73 | 88.15 | 0.74 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 184.86 | 112.49 | 0.67 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 285.53 | 261.50 | 0.92 | 1.50 |  |
| pass | rss.after_open | rocksdb | 145.78 | 64.47 | 0.54 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 175005702.00 | 130150406.00 | 0.74 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41162410.00 | 41140275.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 2000.98 | 7867.38 | 3.93 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 2099.42 | 11487.57 | 5.47 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 81.03 | 5237.71 | 64.64 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 76.33 | 6009.71 | 78.73 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 1616.66 | 3229.70 | 2.00 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 2293.12 | 8368.60 | 3.65 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 85.62 | 2223.09 | 25.96 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 76.53 | 5707.74 | 74.58 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1542.54 | 1586.39 | 1.03 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1449.55 | 1694.83 | 1.17 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 84.87 | 2362.31 | 27.83 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 79.29 | 2436.60 | 30.73 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1211.81 | 1324.94 | 1.09 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1460.59 | 1751.58 | 1.20 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 85.88 | 1382.58 | 16.10 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 76.44 | 2321.10 | 30.37 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 80.25 | 98.35 | 1.23 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 75.32 | 85.98 | 1.14 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 7.17 | 14.90 | 2.08 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 6.30 | 13.65 | 2.17 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 93.56 | 93.79 | 1.00 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 77.89 | 80.94 | 1.04 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 7.45 | 14.56 | 1.95 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 6.68 | 13.54 | 2.03 | – | report-only |

