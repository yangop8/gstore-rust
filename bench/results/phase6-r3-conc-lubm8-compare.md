## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `77f7ed09b24e6bc65e67a5e5a0d7ff2650a4f58a`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.26 2.38 1.78
- calibration score 69.92 ms (sort 44.47, hashmap 134.12, format 57.32, fsync 0.37)
- compared with: reference profile, commit `77f7ed09b24e6bc65e67a5e5a0d7ff2650a4f58a`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.59 | 112.8 | 85.5 | 208.3 | 228.6 | 103.1 | 126.5 | 4.15 | 0.94× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.30 | 0.07 | 1.15× |
| q2 | 25 | 10.65 | 9.64 | 0.99× |
| q3 | 10 | 0.24 | 0.16 | 1.94× |
| q4 | 9 | 0.18 | 0.12 | 0.85× |
| q5 | 576 | 50.21 | 40.14 | 1.27× |
| q6 | 80056 | 31.12 | 30.61 | 1.32× |
| q7 | 23 | 50.97 | 48.34 | 1.42× |
| q8 | 10778 | 36.87 | 35.11 | 0.92× |
| q9 | 1081 | 44.33 | 44.16 | 0.79× |
| q10 | 3 | 23.40 | 23.00 | 0.63× |
| q11 | 325 | 0.61 | 0.27 | 0.46× |
| q12 | 21 | 0.28 | 0.15 | 0.50× |
| q13 | 85670 | 24.02 | 24.42 | 0.95× |
| q14 | 60576 | 16.51 | 16.31 | 0.97× |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 9287.1 | 0.11 | 0.15 | 0 | identical | 1.03× |
| snapshot | sparql | q1 | 8 | 50048.3 | 0.15 | 0.35 | 0 | identical | 3.80× |
| snapshot | sparql | q4 | 1 | 5034.6 | 0.20 | 0.29 | 0 | identical | 1.01× |
| snapshot | sparql | q4 | 8 | 24720.7 | 0.32 | 0.66 | 0 | identical | 3.75× |
| snapshot | api | q1 | 1 | 4490.0 | 0.21 | 0.33 | 0 | identical | 1.00× |
| snapshot | api | q1 | 8 | 13244.5 | 0.59 | 1.27 | 0 | identical | 1.17× |
| snapshot | api | q4 | 1 | 2288.1 | 0.39 | 0.56 | 0 | identical | 0.94× |
| snapshot | api | q4 | 8 | 9289.4 | 0.80 | 1.71 | 0 | identical | 1.59× |

### Gates: PASS (104 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.06 | 0.07 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q2 | 9.71 | 9.82 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.08 | 0.16 | 1.04 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.14 | 0.12 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q5 | 31.51 | 40.89 | 1.28 | 3.00 |  |
| pass | time.query | snapshot/q6 | 23.23 | 31.19 | 1.32 | 3.00 |  |
| pass | time.query | snapshot/q7 | 33.98 | 49.24 | 1.42 | 3.00 |  |
| pass | time.query | snapshot/q8 | 38.27 | 35.77 | 0.94 | 3.00 |  |
| pass | time.query | snapshot/q9 | 55.58 | 44.99 | 0.82 | 3.00 |  |
| pass | time.query | snapshot/q10 | 36.41 | 23.43 | 0.66 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.58 | 0.27 | 0.88 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.31 | 0.16 | 0.93 | 3.00 |  |
| pass | time.query | snapshot/q13 | 25.68 | 24.87 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q14 | 16.80 | 16.61 | 0.99 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 1.02 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | snapshot | 6156.92 | 5691.21 | 0.93 | 2.00 |  |
| pass | time.open | snapshot | 201.20 | 232.91 | 1.14 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 125.74 | 126.52 | 1.00 | 1.50 |  |
| pass | rss.build_peak | snapshot | 112.30 | 112.78 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 101.72 | 103.11 | 1.01 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 9033.84 | 9287.13 | 1.03 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 13185.25 | 50048.35 | 3.80 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 5003.94 | 5034.61 | 1.01 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 6593.45 | 24720.67 | 3.75 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 4480.19 | 4490.05 | 1.00 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 11323.70 | 13244.47 | 1.17 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2437.40 | 2288.11 | 0.94 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 5846.31 | 9289.41 | 1.59 | – | report-only |

