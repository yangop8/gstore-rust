## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.24 2.52 3.31
- calibration score 63.35 ms (sort 45.24, hashmap 110.51, format 50.85, fsync 0.33)
- compared with: reference profile, commit `05be7e3d58b27605c7e1eb2680039e52fb961743`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.80 | 112.4 | 85.5 | 213.4 (cold) | 176.8 | 101.7 | 126.8 | 3.29 | 1.08× |
| disk | 4.04 | 96.9 | 130.2 | 294.5 (cold) | 258.8 | 88.2 | 118.3 | 5.87 | 1.04× |
| rocksdb | 35.60 | 261.5 | 41.1 | 202.3 (cold) | 180.4 | 64.5 | 112.5 | 33.28 | 1.07× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.24 | 0.05 | 1.14× | 2.65 | 0.38 | 1.06× | 11.17 | 9.72 | 1.13× |
| q2 | 25 | 7.78 | 6.08 | 1.03× | 10.07 | 8.37 | 0.97× | 40.64 | 36.01 | 1.08× |
| q3 | 10 | 0.20 | 0.12 | 1.23× | 1.39 | 0.47 | 1.10× | 30.91 | 29.44 | 1.05× |
| q4 | 9 | 0.10 | 0.06 | 1.06× | 0.54 | 0.18 | 1.00× | 70.04 | 59.02 | 1.03× |
| q5 | 576 | 34.51 | 24.69 | 1.07× | 41.66 | 33.23 | 1.05× | 56.47 | 51.83 | 1.05× |
| q6 | 80056 | 21.98 | 21.07 | 1.06× | 30.43 | 30.43 | 1.07× | 50.89 | 50.70 | 1.10× |
| q7 | 23 | 32.02 | 30.89 | 1.01× | 52.79 | 53.01 | 1.07× | 112.25 | 103.85 | 1.00× |
| q8 | 10778 | 34.94 | 35.68 | 1.11× | 48.96 | 51.07 | 1.08× | 137.72 | 135.20 | 1.07× |
| q9 | 1081 | 47.18 | 45.82 | 1.07× | 67.61 | 72.68 | 1.11× | 233.61 | 217.16 | 1.02× |
| q10 | 3 | 23.21 | 22.64 | 1.03× | 34.81 | 32.74 | 0.97× | 53.89 | 54.62 | 1.07× |
| q11 | 325 | 0.60 | 0.25 | 1.05× | 0.86 | 0.33 | 1.00× | 3.25 | 2.46 | 1.07× |
| q12 | 21 | 0.23 | 0.13 | 0.97× | 0.35 | 0.12 | 0.98× | 1.72 | 1.37 | 1.13× |
| q13 | 85670 | 24.23 | 24.83 | 1.12× | 39.68 | 41.35 | 1.09× | 53.98 | 55.15 | 1.10× |
| q14 | 60576 | 16.35 | 16.55 | 1.16× | 20.67 | 20.12 | 1.05× | 41.47 | 41.23 | 1.09× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.22 | 0.01 / 0.01 | 5330 | 52.5 | 29.4 | 319.0 | 181.8 | 168432 → 169456 | ok |
| disk | 1.69 / 2.50 | 1.36 / 2.37 | 5330 | 835.8 | 877.9 | 0.1 | 637.4 | 168432 → 169456 | ok |
| rocksdb | 0.25 / 0.36 | 0.21 / 0.27 | 5330 | 156.1 | 106.1 | 27.4 | 152.9 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 7867.4 | 0.13 | 0.18 | 0 | identical | 0.84× |
| snapshot | sparql | q1 | 8 | 11487.6 | 0.70 | 1.40 | 0 | identical | 0.81× |
| snapshot | sparql | q4 | 1 | 5237.7 | 0.19 | 0.32 | 0 | identical | 0.99× |
| snapshot | sparql | q4 | 8 | 6009.7 | 1.33 | 2.67 | 0 | identical | 0.84× |
| snapshot | api | q1 | 1 | 3229.7 | 0.30 | 0.49 | 0 | identical | 0.72× |
| snapshot | api | q1 | 8 | 8368.6 | 0.94 | 1.89 | 0 | identical | 0.89× |
| snapshot | api | q4 | 1 | 2223.1 | 0.40 | 0.61 | 0 | identical | 1.02× |
| snapshot | api | q4 | 8 | 5707.7 | 1.34 | 2.68 | 0 | identical | 1.00× |
| disk | sparql | q1 | 1 | 1586.4 | 0.63 | 0.83 | 0 | identical | 0.86× |
| disk | sparql | q1 | 8 | 1694.8 | 4.72 | 5.94 | 0 | identical | 0.97× |
| disk | sparql | q4 | 1 | 2362.3 | 0.42 | 0.59 | 0 | identical | 0.96× |
| disk | sparql | q4 | 8 | 2436.6 | 3.28 | 4.02 | 0 | identical | 1.00× |
| disk | api | q1 | 1 | 1324.9 | 0.74 | 1.01 | 0 | identical | 1.02× |
| disk | api | q1 | 8 | 1751.6 | 4.55 | 5.78 | 0 | identical | 1.05× |
| disk | api | q4 | 1 | 1382.6 | 0.67 | 1.04 | 0 | identical | 0.94× |
| disk | api | q4 | 8 | 2321.1 | 3.38 | 6.30 | 0 | identical | 0.97× |
| rocksdb | sparql | q1 | 1 | 98.3 | 10.17 | 16.19 | 0 | identical | 1.13× |
| rocksdb | sparql | q1 | 8 | 86.0 | 92.27 | 116.22 | 0 | identical | 1.10× |
| rocksdb | sparql | q4 | 1 | 14.9 | 67.12 | 100.98 | 0 | identical | 1.00× |
| rocksdb | sparql | q4 | 8 | 13.6 | 558.29 | 672.33 | 0 | identical | 0.99× |
| rocksdb | api | q1 | 1 | 93.8 | 10.64 | 16.12 | 0 | identical | 0.94× |
| rocksdb | api | q1 | 8 | 80.9 | 98.01 | 119.99 | 0 | identical | 0.89× |
| rocksdb | api | q4 | 1 | 14.6 | 68.57 | 99.88 | 0 | identical | 0.96× |
| rocksdb | api | q4 | 8 | 13.5 | 565.99 | 692.26 | 0 | identical | 1.10× |

### Gates: PASS (281 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.04 | 0.05 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q2 | 5.89 | 5.83 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.09 | 0.11 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.06 | 0.06 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q5 | 23.06 | 23.66 | 1.02 | 3.00 |  |
| pass | time.query | snapshot/q6 | 19.92 | 20.20 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q7 | 30.69 | 29.60 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q8 | 32.04 | 34.19 | 1.06 | 3.00 |  |
| pass | time.query | snapshot/q9 | 42.72 | 43.91 | 1.03 | 3.00 |  |
| pass | time.query | snapshot/q10 | 22.06 | 21.70 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.24 | 0.24 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.14 | 0.13 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q13 | 22.21 | 23.80 | 1.07 | 3.00 |  |
| pass | time.query | snapshot/q14 | 14.30 | 15.86 | 1.10 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 1.03 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | snapshot | 5334.51 | 5555.23 | 1.04 | 2.00 |  |
| pass | time.open | snapshot | 243.30 | 169.44 | 0.72 | 2.50 |  |
| pass | time.query | disk/q1 | 0.36 | 0.37 | 1.00 | 3.00 |  |
| pass | time.query | disk/q2 | 8.60 | 8.02 | 0.95 | 3.00 |  |
| pass | time.query | disk/q3 | 0.43 | 0.45 | 1.01 | 3.00 |  |
| pass | time.query | disk/q4 | 0.18 | 0.17 | 1.00 | 3.00 |  |
| pass | time.query | disk/q5 | 31.61 | 31.85 | 1.01 | 3.00 |  |
| pass | time.query | disk/q6 | 28.36 | 29.17 | 1.03 | 3.00 |  |
| pass | time.query | disk/q7 | 49.72 | 50.80 | 1.02 | 3.00 |  |
| pass | time.query | disk/q8 | 47.36 | 48.94 | 1.03 | 3.00 |  |
| pass | time.query | disk/q9 | 65.40 | 69.65 | 1.06 | 3.00 |  |
| pass | time.query | disk/q10 | 33.62 | 31.37 | 0.94 | 3.00 |  |
| pass | time.query | disk/q11 | 0.33 | 0.31 | 0.99 | 3.00 |  |
| pass | time.query | disk/q12 | 0.13 | 0.12 | 1.00 | 3.00 |  |
| pass | time.query | disk/q13 | 37.90 | 39.63 | 1.04 | 3.00 |  |
| pass | time.query | disk/q14 | 19.11 | 19.29 | 1.01 | 3.00 |  |
| pass | time.geomean | disk | – | – | 1.01 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk | 90141.56 | 3867.52 | 0.04 | 2.50 |  |
| pass | time.open | disk | 317.54 | 248.05 | 0.79 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 8.61 | 9.45 | 1.08 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 33.47 | 35.02 | 1.04 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 28.06 | 28.64 | 1.02 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 57.54 | 57.40 | 1.00 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 49.56 | 50.41 | 1.02 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 45.91 | 49.31 | 1.07 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 103.41 | 101.01 | 0.98 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 126.00 | 131.50 | 1.04 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 212.45 | 211.20 | 0.99 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 50.89 | 53.12 | 1.04 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.30 | 2.39 | 1.02 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.22 | 1.33 | 1.04 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 50.33 | 53.64 | 1.06 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 37.77 | 40.10 | 1.06 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 1.04 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 31494.87 | 34623.61 | 1.10 | 2.00 |  |
| pass | time.open | rocksdb | 291.64 | 175.50 | 0.63 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 154.04 | 126.75 | 0.85 | 1.50 |  |
| pass | rss.build_peak | snapshot | 138.11 | 112.43 | 0.85 | 1.50 |  |
| pass | rss.after_open | snapshot | 128.07 | 101.67 | 0.84 | 1.50 |  |
| pass | rss.query_hwm | disk | 145.19 | 118.30 | 0.85 | 1.50 |  |
| pass | rss.build_peak | disk | 123.09 | 96.86 | 0.83 | 1.50 |  |
| pass | rss.after_open | disk | 114.50 | 88.15 | 0.82 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 138.76 | 112.49 | 0.85 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 286.77 | 261.50 | 0.92 | 1.50 |  |
| pass | rss.after_open | rocksdb | 90.22 | 64.47 | 0.79 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 175005701.00 | 130150406.00 | 0.74 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41135741.00 | 41140275.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 9336.07 | 7867.38 | 0.84 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 14209.92 | 11487.57 | 0.81 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 5309.53 | 5237.71 | 0.99 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 7180.39 | 6009.71 | 0.84 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 4463.93 | 3229.70 | 0.72 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 9418.45 | 8368.60 | 0.89 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2176.91 | 2223.09 | 1.02 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 5697.36 | 5707.74 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1851.17 | 1586.39 | 0.86 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1739.10 | 1694.83 | 0.97 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 2470.85 | 2362.31 | 0.96 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 2439.24 | 2436.60 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1295.95 | 1324.94 | 1.02 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1675.79 | 1751.58 | 1.05 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 1472.86 | 1382.58 | 0.94 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 2392.26 | 2321.10 | 0.97 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 87.23 | 98.35 | 1.13 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 78.30 | 85.98 | 1.10 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 14.89 | 14.90 | 1.00 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 13.75 | 13.65 | 0.99 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 100.28 | 93.79 | 0.94 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 90.98 | 80.94 | 0.89 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 15.20 | 14.56 | 0.96 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 12.34 | 13.54 | 1.10 | – | report-only |

