# Stream-mode disk (phase6-r3-disk-stream-lubm8.json) against Materialize-mode disk of the same commit (phase6-r3-lubm8.json, --backends disk)

## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.77 1.74 1.60
- calibration score 64.67 ms (sort 45.26, hashmap 112.00, format 53.35, fsync 0.29)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk | 3.73 | 108.9 | 130.2 | 4.4 (cold) | 0.6 | 5.9 | 69.2 | 6.67 | 1.21× |

| query | rows | disk first ms | disk median ms | disk vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 3.40 | 0.42 | 1.18× |
| q2 | 25 | 15.58 | 10.97 | 1.31× |
| q3 | 10 | 2.47 | 0.47 | 1.15× |
| q4 | 9 | 0.57 | 0.21 | 1.20× |
| q5 | 576 | 42.99 | 34.97 | 1.07× |
| q6 | 80056 | 58.07 | 35.30 | 1.24× |
| q7 | 23 | 54.99 | 44.54 | 0.99× |
| q8 | 10778 | 54.48 | 59.15 | 1.37× |
| q9 | 1081 | 99.49 | 88.38 | 1.38× |
| q10 | 3 | 33.65 | 30.84 | 1.02× |
| q11 | 325 | 1.18 | 0.43 | 1.36× |
| q12 | 21 | 0.46 | 0.17 | 1.32× |
| q13 | 85670 | 51.02 | 47.42 | 1.25× |
| q14 | 60576 | 25.89 | 24.76 | 1.25× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk | 2.03 / 3.40 | 1.46 / 2.36 | 5330 | 817.7 | 1062.8 | 0.0 | 1.0 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk | sparql | q1 | 1 | 1792.7 | 0.56 | 0.75 | 0 | identical | 0.93× |
| disk | sparql | q1 | 8 | 4625.2 | 1.73 | 3.12 | 0 | identical | 0.98× |
| disk | sparql | q4 | 1 | 2293.8 | 0.43 | 0.67 | 0 | identical | 0.87× |
| disk | sparql | q4 | 8 | 7694.0 | 1.03 | 2.06 | 0 | identical | 0.94× |
| disk | api | q1 | 1 | 1425.1 | 0.69 | 0.96 | 0 | identical | 0.96× |
| disk | api | q1 | 8 | 4212.4 | 1.88 | 3.65 | 0 | identical | 1.00× |
| disk | api | q4 | 1 | 1520.2 | 0.61 | 0.90 | 0 | identical | 0.92× |
| disk | api | q4 | 8 | 4247.2 | 1.81 | 3.62 | 0 | identical | 0.70× |

### Gates: PASS (105 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk/q1 | 0.36 | 0.37 | 1.01 | 3.00 |  |
| pass | time.query | disk/q2 | 8.36 | 9.74 | 1.13 | 3.00 |  |
| pass | time.query | disk/q3 | 0.41 | 0.42 | 1.00 | 3.00 |  |
| pass | time.query | disk/q4 | 0.18 | 0.19 | 1.01 | 3.00 |  |
| pass | time.query | disk/q5 | 32.72 | 31.05 | 0.95 | 3.00 |  |
| pass | time.query | disk/q6 | 28.36 | 31.35 | 1.10 | 3.00 |  |
| pass | time.query | disk/q7 | 45.21 | 39.56 | 0.88 | 3.00 |  |
| pass | time.query | disk/q8 | 43.08 | 52.53 | 1.21 | 3.00 |  |
| pass | time.query | disk/q9 | 64.05 | 78.49 | 1.22 | 3.00 |  |
| pass | time.query | disk/q10 | 30.22 | 27.39 | 0.91 | 3.00 |  |
| pass | time.query | disk/q11 | 0.32 | 0.39 | 1.03 | 3.00 |  |
| pass | time.query | disk/q12 | 0.13 | 0.15 | 1.01 | 3.00 |  |
| pass | time.query | disk/q13 | 38.05 | 42.11 | 1.10 | 3.00 |  |
| pass | time.query | disk/q14 | 19.75 | 21.98 | 1.10 | 3.00 |  |
| pass | time.geomean | disk | – | – | 1.07 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk | 4493.37 | 3314.45 | 0.74 | 2.50 |  |
| pass | time.open | disk | 259.05 | 0.56 | 0.07 | 2.50 |  |
| pass | rss.query_hwm | disk | 119.25 | 69.21 | 0.67 | 1.50 |  |
| pass | rss.build_peak | disk | 98.72 | 108.86 | 1.08 | 1.50 |  |
| pass | rss.after_open | disk | 88.63 | 5.93 | 0.31 | 1.50 |  |
| pass | bytes.dir | disk | 130150405.00 | 130150405.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1923.50 | 1792.72 | 0.93 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 4698.40 | 4625.19 | 0.98 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 2629.36 | 2293.81 | 0.87 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 8206.31 | 7694.01 | 0.94 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1485.28 | 1425.11 | 0.96 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 4212.98 | 4212.39 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 1652.34 | 1520.17 | 0.92 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 6046.12 | 4247.16 | 0.70 | – | report-only |

exit: 0

# Stream-mode disk against the pre-Phase-6 disk layout (pre-phase6-lubm8.json, --backends disk)

## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 2.77 1.74 1.60
- calibration score 64.67 ms (sort 45.26, hashmap 112.00, format 53.35, fsync 0.29)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk | 3.73 | 108.9 | 130.2 | 4.4 (cold) | 0.6 | 5.9 | 69.2 | 6.67 | 0.35× |

| query | rows | disk first ms | disk median ms | disk vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 3.40 | 0.42 | 1.00× |
| q2 | 25 | 15.58 | 10.97 | 0.29× |
| q3 | 10 | 2.47 | 0.47 | 0.98× |
| q4 | 9 | 0.57 | 0.21 | 0.02× |
| q5 | 576 | 42.99 | 34.97 | 0.30× |
| q6 | 80056 | 58.07 | 35.30 | 0.36× |
| q7 | 23 | 54.99 | 44.54 | 0.21× |
| q8 | 10778 | 54.48 | 59.15 | 0.36× |
| q9 | 1081 | 99.49 | 88.38 | 0.33× |
| q10 | 3 | 33.65 | 30.84 | 0.27× |
| q11 | 325 | 1.18 | 0.43 | 0.80× |
| q12 | 21 | 0.46 | 0.17 | 1.08× |
| q13 | 85670 | 51.02 | 47.42 | 0.28× |
| q14 | 60576 | 25.89 | 24.76 | 0.41× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk | 2.03 / 3.40 | 1.46 / 2.36 | 5330 | 817.7 | 1062.8 | 0.0 | 1.0 | 168432 → 169456 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk | sparql | q1 | 1 | 1792.7 | 0.56 | 0.75 | 0 | identical | 1.16× |
| disk | sparql | q1 | 8 | 4625.2 | 1.73 | 3.12 | 0 | identical | 3.19× |
| disk | sparql | q4 | 1 | 2293.8 | 0.43 | 0.67 | 0 | identical | 27.03× |
| disk | sparql | q4 | 8 | 7694.0 | 1.03 | 2.06 | 0 | identical | 97.04× |
| disk | api | q1 | 1 | 1425.1 | 0.69 | 0.96 | 0 | identical | 1.18× |
| disk | api | q1 | 8 | 4212.4 | 1.88 | 3.65 | 0 | identical | 2.88× |
| disk | api | q4 | 1 | 1520.2 | 0.61 | 0.90 | 0 | identical | 17.70× |
| disk | api | q4 | 8 | 4247.2 | 1.81 | 3.62 | 0 | identical | 55.57× |

### Gates: PASS (105 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk/q1 | 0.42 | 0.44 | 1.01 | 3.00 |  |
| pass | time.query | disk/q2 | 38.39 | 11.43 | 0.33 | 3.00 |  |
| pass | time.query | disk/q3 | 0.48 | 0.49 | 1.00 | 3.00 |  |
| pass | time.query | disk/q4 | 9.73 | 0.22 | 0.19 | 3.00 |  |
| pass | time.query | disk/q5 | 115.47 | 36.46 | 0.33 | 3.00 |  |
| pass | time.query | disk/q6 | 97.27 | 36.80 | 0.39 | 3.00 |  |
| pass | time.query | disk/q7 | 216.50 | 46.44 | 0.22 | 3.00 |  |
| pass | time.query | disk/q8 | 164.92 | 61.68 | 0.38 | 3.00 |  |
| pass | time.query | disk/q9 | 266.26 | 92.15 | 0.35 | 3.00 |  |
| pass | time.query | disk/q10 | 114.01 | 32.16 | 0.29 | 3.00 |  |
| pass | time.query | disk/q11 | 0.54 | 0.45 | 0.97 | 3.00 |  |
| pass | time.query | disk/q12 | 0.15 | 0.17 | 1.01 | 3.00 |  |
| pass | time.query | disk/q13 | 172.05 | 49.44 | 0.30 | 3.00 |  |
| pass | time.query | disk/q14 | 60.17 | 25.81 | 0.45 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.25 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk | 263743.67 | 3891.33 | 0.02 | 2.50 |  |
| pass | time.open | disk | 1447.34 | 0.66 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | disk | 167.48 | 69.21 | 0.51 | 1.50 |  |
| pass | rss.build_peak | disk | 164.71 | 108.86 | 0.72 | 1.50 |  |
| pass | rss.after_open | disk | 129.73 | 5.93 | 0.23 | 1.50 |  |
| pass | bytes.dir | disk | 175005702.00 | 130150405.00 | 0.74 | 1.25 |  |
| pass | info.conc_qps | disk/sparql/q1/c1 | 1542.54 | 1792.72 | 1.16 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 1449.55 | 4625.19 | 3.19 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 84.87 | 2293.81 | 27.03 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 79.29 | 7694.01 | 97.04 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 1211.81 | 1425.11 | 1.18 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 1460.59 | 4212.39 | 2.88 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 85.88 | 1520.17 | 17.70 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 76.44 | 4247.16 | 55.57 | – | report-only |

exit: 0
