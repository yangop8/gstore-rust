## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `77f7ed09b24e6bc65e67a5e5a0d7ff2650a4f58a`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.02 2.82 2.40
- calibration score 64.36 ms (sort 43.55, hashmap 104.41, format 58.62, fsync 0.40)
- compared with: reference profile, commit `77f7ed09b24e6bc65e67a5e5a0d7ff2650a4f58a`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 30.02 | 550.3 | 430.8 | 1168.1 | 1045.2 | 489.9 | 589.3 | 22.84 | 1.08× |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.36 | 0.20 | 1.12× |
| q2 | 115 | 43.28 | 55.26 | 1.30× |
| q3 | 10 | 1.02 | 0.79 | 1.38× |
| q4 | 9 | 0.22 | 0.15 | 1.31× |
| q5 | 576 | 367.98 | 220.39 | 0.88× |
| q6 | 404399 | 129.77 | 124.33 | 1.01× |
| q7 | 23 | 376.49 | 396.87 | 1.35× |
| q8 | 10778 | 302.75 | 358.97 | 1.04× |
| q9 | 5507 | 347.15 | 390.04 | 0.93× |
| q10 | 3 | 399.07 | 299.10 | 1.35× |
| q11 | 325 | 2.46 | 1.21 | 0.90× |
| q12 | 21 | 1.11 | 0.96 | 1.02× |
| q13 | 432512 | 162.63 | 165.72 | 0.95× |
| q14 | 305503 | 92.19 | 92.93 | 0.76× |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 3880.7 | 0.26 | 0.38 | 0 | identical | 0.99× |
| snapshot | sparql | q1 | 8 | 20007.4 | 0.39 | 0.86 | 0 | identical | 5.01× |
| snapshot | sparql | q4 | 1 | 3892.9 | 0.25 | 0.38 | 0 | identical | 1.06× |
| snapshot | sparql | q4 | 8 | 20056.3 | 0.39 | 0.83 | 0 | identical | 4.29× |
| snapshot | api | q1 | 1 | 2391.9 | 0.40 | 0.56 | 0 | identical | 0.94× |
| snapshot | api | q1 | 8 | 9523.1 | 0.82 | 1.76 | 0 | identical | 2.22× |
| snapshot | api | q4 | 1 | 2094.4 | 0.43 | 0.62 | 0 | identical | 1.03× |
| snapshot | api | q4 | 8 | 8688.9 | 0.86 | 1.84 | 0 | identical | 1.81× |

### Gates: PASS (104 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.18 | 0.19 | 1.00 | 3.00 |  |
| pass | time.query | snapshot/q2 | 42.53 | 52.19 | 1.22 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.57 | 0.75 | 1.07 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.11 | 0.14 | 1.01 | 3.00 |  |
| pass | time.query | snapshot/q5 | 249.20 | 208.15 | 0.84 | 3.00 |  |
| pass | time.query | snapshot/q6 | 122.67 | 117.43 | 0.96 | 3.00 |  |
| pass | time.query | snapshot/q7 | 293.08 | 374.83 | 1.28 | 3.00 |  |
| pass | time.query | snapshot/q8 | 344.26 | 339.04 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q9 | 417.86 | 368.38 | 0.88 | 3.00 |  |
| pass | time.query | snapshot/q10 | 221.58 | 282.49 | 1.27 | 3.00 |  |
| pass | time.query | snapshot/q11 | 1.35 | 1.14 | 0.94 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.94 | 0.91 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q13 | 173.71 | 156.52 | 0.90 | 3.00 |  |
| pass | time.query | snapshot/q14 | 121.81 | 87.77 | 0.73 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.97 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 30649.47 | 28353.79 | 0.93 | 2.00 |  |
| pass | time.open | snapshot | 1172.91 | 987.21 | 0.84 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 582.56 | 589.27 | 1.01 | 1.50 |  |
| pass | rss.build_peak | snapshot | 549.91 | 550.27 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 489.03 | 489.91 | 1.00 | 1.50 |  |
| pass | bytes.dir | snapshot | 430848180.00 | 430848180.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 3914.24 | 3880.70 | 0.99 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 3992.38 | 20007.39 | 5.01 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 3677.18 | 3892.89 | 1.06 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 4678.67 | 20056.35 | 4.29 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 2552.41 | 2391.91 | 0.94 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 4289.32 | 9523.09 | 2.22 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2026.84 | 2094.38 | 1.03 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 4810.00 | 8688.87 | 1.81 | – | report-only |

