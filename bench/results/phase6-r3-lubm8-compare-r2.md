## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.78 2.24 1.81
- calibration score 56.34 ms (sort 42.62, hashmap 93.21, format 45.01, fsync 0.26)
- compared with: reference profile, commit `0c9bdb6eb68f7433a5a22fbcfe3ec0e9c731f4da`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.31 | 112.7 | 85.5 | 209.0 (cold) | 187.1 | 102.0 | 128.8 | 3.11 | 0.94× |
| disk | 4.49 | 98.7 | 130.2 | 288.9 (cold) | 259.1 | 88.6 | 119.2 | 5.49 | 0.94× |
| rocksdb | 4.21 | 93.4 | 31.3 | 255.2 (cold) | 184.3 | 68.3 | 102.6 | 10.56 | 0.32× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk first ms | disk median ms | disk vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.23 | 0.03 | 0.73× | 2.63 | 0.36 | 0.93× | 2.48 | 1.89 | 0.19× |
| q2 | 25 | 7.74 | 6.03 | 0.99× | 10.24 | 8.36 | 1.00× | 8.04 | 5.80 | 0.16× |
| q3 | 10 | 0.16 | 0.07 | 0.63× | 1.31 | 0.41 | 0.87× | 7.44 | 6.02 | 0.20× |
| q4 | 9 | 0.10 | 0.07 | 1.12× | 0.50 | 0.18 | 1.01× | 0.59 | 0.47 | 0.01× |
| q5 | 576 | 33.42 | 24.74 | 1.00× | 40.45 | 32.72 | 0.98× | 50.20 | 38.84 | 0.75× |
| q6 | 80056 | 22.27 | 21.93 | 1.04× | 29.39 | 28.36 | 0.93× | 39.18 | 39.45 | 0.78× |
| q7 | 23 | 37.46 | 30.47 | 0.99× | 47.89 | 45.21 | 0.85× | 47.70 | 46.14 | 0.44× |
| q8 | 10778 | 32.40 | 34.11 | 0.96× | 42.37 | 43.08 | 0.84× | 94.21 | 93.06 | 0.69× |
| q9 | 1081 | 45.25 | 44.27 | 0.97× | 63.05 | 64.05 | 0.88× | 115.65 | 106.81 | 0.49× |
| q10 | 3 | 23.74 | 22.64 | 1.00× | 32.72 | 30.22 | 0.92× | 38.42 | 38.30 | 0.70× |
| q11 | 325 | 0.60 | 0.24 | 0.97× | 0.77 | 0.32 | 0.98× | 1.61 | 1.04 | 0.42× |
| q12 | 21 | 0.23 | 0.13 | 1.00× | 0.31 | 0.13 | 1.02× | 0.29 | 0.19 | 0.14× |
| q13 | 85670 | 24.14 | 24.45 | 0.98× | 38.01 | 38.05 | 0.92× | 42.59 | 42.35 | 0.77× |
| q14 | 60576 | 15.35 | 16.10 | 0.97× | 19.10 | 19.75 | 0.98× | 30.21 | 30.65 | 0.74× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.17 | 0.00 / 0.01 | 5330 | 58.8 | 33.7 | 294.0 | 163.4 | 168432 → 169456 | ok |
| disk | 1.42 / 2.44 | 1.01 / 1.20 | 5330 | 606.0 | 759.1 | 0.0 | 568.6 | 168432 → 169456 | ok |
| rocksdb | 0.22 / 0.54 | 0.17 / 0.28 | 5330 | 119.2 | 103.8 | 29.0 | 151.1 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 10173.2 | 0.10 | 0.14 | 0 | identical | 1.29× |
| snapshot | sparql | q1 | 8 | 55390.7 | 0.14 | 0.31 | 0 | identical | 4.82× |
| snapshot | sparql | q4 | 1 | 5038.1 | 0.20 | 0.29 | 0 | identical | 0.96× |
| snapshot | sparql | q4 | 8 | 27252.9 | 0.29 | 0.61 | 0 | identical | 4.53× |
| snapshot | api | q1 | 1 | 5034.7 | 0.19 | 0.33 | 0 | identical | 1.56× |
| snapshot | api | q1 | 8 | 14788.1 | 0.53 | 1.17 | 0 | identical | 1.77× |
| snapshot | api | q4 | 1 | 2852.5 | 0.31 | 0.49 | 0 | identical | 1.28× |
| snapshot | api | q4 | 8 | 11469.6 | 0.65 | 1.35 | 0 | identical | 2.01× |
| disk | sparql | q1 | 1 | 1923.5 | 0.52 | 0.72 | 0 | identical | 1.21× |
| disk | sparql | q1 | 8 | 4698.4 | 1.70 | 3.16 | 0 | identical | 2.77× |
| disk | sparql | q4 | 1 | 2629.4 | 0.38 | 0.56 | 0 | identical | 1.11× |
| disk | sparql | q4 | 8 | 8206.3 | 0.97 | 1.92 | 0 | identical | 3.37× |
| disk | api | q1 | 1 | 1485.3 | 0.66 | 0.89 | 0 | identical | 1.12× |
| disk | api | q1 | 8 | 4213.0 | 1.88 | 3.51 | 0 | identical | 2.41× |
| disk | api | q4 | 1 | 1652.3 | 0.56 | 0.79 | 0 | identical | 1.20× |
| disk | api | q4 | 8 | 6046.1 | 1.27 | 2.50 | 0 | identical | 2.60× |

### Gates: FAIL (exact) (273 checked, 1 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| **FAIL** | exact.phase | rocksdb/conc | – | – | – | – | the baseline ran the concurrency phase; the result did not |
| pass | time.query | snapshot/q1 | 0.05 | 0.04 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q2 | 6.08 | 6.78 | 1.09 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.12 | 0.08 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.06 | 0.08 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q5 | 24.69 | 27.82 | 1.12 | 3.00 |  |
| pass | time.query | snapshot/q6 | 21.07 | 24.65 | 1.16 | 3.00 |  |
| pass | time.query | snapshot/q7 | 30.89 | 34.26 | 1.10 | 3.00 |  |
| pass | time.query | snapshot/q8 | 35.68 | 38.35 | 1.07 | 3.00 |  |
| pass | time.query | snapshot/q9 | 45.82 | 49.77 | 1.08 | 3.00 |  |
| pass | time.query | snapshot/q10 | 22.64 | 25.46 | 1.11 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.25 | 0.27 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.13 | 0.15 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q13 | 24.83 | 27.50 | 1.10 | 3.00 |  |
| pass | time.query | snapshot/q14 | 16.55 | 18.11 | 1.08 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 1.11 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | snapshot | 5796.54 | 5969.21 | 1.03 | 2.00 |  |
| pass | time.open | snapshot | 176.80 | 210.38 | 1.17 | 2.50 |  |
| pass | time.query | disk/q1 | 0.38 | 0.39 | 1.00 | 3.00 |  |
| pass | time.query | disk/q2 | 8.37 | 9.22 | 1.08 | 3.00 |  |
| pass | time.query | disk/q3 | 0.47 | 0.45 | 0.99 | 3.00 |  |
| pass | time.query | disk/q4 | 0.18 | 0.20 | 1.01 | 3.00 |  |
| pass | time.query | disk/q5 | 33.23 | 36.10 | 1.08 | 3.00 |  |
| pass | time.query | disk/q6 | 30.43 | 31.28 | 1.03 | 3.00 |  |
| pass | time.query | disk/q7 | 53.01 | 49.87 | 0.94 | 3.00 |  |
| pass | time.query | disk/q8 | 51.07 | 47.53 | 0.93 | 3.00 |  |
| pass | time.query | disk/q9 | 72.68 | 70.65 | 0.97 | 3.00 |  |
| pass | time.query | disk/q10 | 32.74 | 33.34 | 1.02 | 3.00 |  |
| pass | time.query | disk/q11 | 0.33 | 0.35 | 1.01 | 3.00 |  |
| pass | time.query | disk/q12 | 0.12 | 0.14 | 1.01 | 3.00 |  |
| pass | time.query | disk/q13 | 41.35 | 41.97 | 1.01 | 3.00 |  |
| pass | time.query | disk/q14 | 20.12 | 21.78 | 1.07 | 3.00 |  |
| pass | time.geomean | disk | – | – | 1.02 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk | 4035.52 | 4956.46 | 1.22 | 2.50 |  |
| pass | time.open | disk | 258.82 | 285.75 | 1.10 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.72 | 1.91 | 0.33 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 36.01 | 5.87 | 0.21 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 29.44 | 6.09 | 0.26 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 59.02 | 0.47 | 0.04 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 51.83 | 39.32 | 0.77 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 50.70 | 39.94 | 0.80 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 103.85 | 46.71 | 0.46 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 135.20 | 94.21 | 0.70 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 217.16 | 108.13 | 0.50 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 54.62 | 38.77 | 0.72 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.46 | 1.06 | 0.69 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.37 | 0.19 | 0.65 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 55.15 | 42.87 | 0.79 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 41.23 | 31.03 | 0.76 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.32 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 35599.73 | 4264.02 | 0.12 | 2.00 |  |
| pass | time.open | rocksdb | 180.44 | 186.62 | 1.03 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 126.75 | 128.79 | 1.01 | 1.50 |  |
| pass | rss.build_peak | snapshot | 112.43 | 112.67 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 101.67 | 101.97 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk | 118.30 | 119.25 | 1.01 | 1.50 |  |
| pass | rss.build_peak | disk | 96.86 | 98.72 | 1.01 | 1.50 |  |
| pass | rss.after_open | disk | 88.15 | 88.63 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 112.49 | 102.61 | 0.93 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 261.50 | 93.36 | 0.43 | 1.50 |  |
| pass | rss.after_open | rocksdb | 64.47 | 68.27 | 1.04 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk | 130150406.00 | 130150405.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41140275.00 | 31266467.00 | 0.76 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 7867.38 | 10173.24 | 1.29 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 11487.57 | 55390.72 | 4.82 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 5237.71 | 5038.13 | 0.96 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 6009.71 | 27252.85 | 4.53 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 3229.70 | 5034.70 | 1.56 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 8368.60 | 14788.13 | 1.77 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2223.09 | 2852.46 | 1.28 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 5707.74 | 11469.60 | 2.01 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1586.39 | 1923.50 | 1.21 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1694.83 | 4698.40 | 2.77 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 2362.31 | 2629.36 | 1.11 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 2436.60 | 8206.31 | 3.37 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1324.94 | 1485.28 | 1.12 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1751.58 | 4212.98 | 2.41 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 1382.58 | 1652.34 | 1.20 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 2321.10 | 6046.12 | 2.60 | – | report-only |

