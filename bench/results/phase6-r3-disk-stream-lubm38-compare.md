# Stream-mode disk (phase6-r3-disk-stream-lubm38.json) against Materialize-mode disk of the same commit (phase6-r3-lubm38.json, --backends disk)

## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.17 1.08 1.51
- calibration score 59.27 ms (sort 44.92, hashmap 101.58, format 45.63, fsync 0.33)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk | 20.06 | 428.7 | 654.0 | 10.5 (cold) | 0.6 | 5.8 | 210.8 | 44.13 | 1.11× |

| query | rows | disk first ms | disk median ms | disk vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 10.19 | 4.14 | 0.98× |
| q2 | 115 | 81.49 | 72.75 | 1.43× |
| q3 | 10 | 11.86 | 4.89 | 1.06× |
| q4 | 9 | 2.82 | 2.15 | 1.01× |
| q5 | 576 | 311.40 | 242.66 | 0.92× |
| q6 | 404399 | 327.62 | 215.59 | 1.32× |
| q7 | 23 | 363.50 | 346.40 | 0.97× |
| q8 | 10778 | 304.91 | 308.28 | 0.83× |
| q9 | 5507 | 566.43 | 591.06 | 1.20× |
| q10 | 3 | 258.19 | 263.86 | 0.95× |
| q11 | 325 | 3.64 | 1.33 | 1.38× |
| q12 | 21 | 2.29 | 0.66 | 1.21× |
| q13 | 432512 | 303.21 | 283.39 | 1.19× |
| q14 | 305503 | 149.00 | 153.70 | 1.29× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk | 1.05 / 1.82 | 0.88 / 1.16 | 6478 | 893.4 | 934.6 | 0.0 | 0.7 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk | sparql | q1 | 1 | 194.7 | 5.13 | 6.97 | 0 | identical | 1.00× |
| disk | sparql | q1 | 8 | 430.2 | 18.60 | 27.57 | 0 | identical | 0.97× |
| disk | sparql | q4 | 1 | 379.8 | 2.63 | 3.47 | 0 | identical | 1.00× |
| disk | sparql | q4 | 8 | 953.5 | 8.38 | 10.89 | 0 | identical | 1.04× |
| disk | api | q1 | 1 | 185.2 | 5.38 | 7.33 | 0 | identical | 0.92× |
| disk | api | q1 | 8 | 443.8 | 18.00 | 26.05 | 0 | identical | 1.07× |
| disk | api | q4 | 1 | 355.2 | 2.76 | 3.95 | 0 | identical | 1.00× |
| disk | api | q4 | 8 | 934.3 | 8.50 | 10.91 | 0 | identical | 1.03× |

### Gates: PASS (105 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk/q1 | 4.22 | 4.30 | 1.01 | 3.00 |  |
| pass | time.query | disk/q2 | 51.03 | 75.41 | 1.46 | 3.00 |  |
| pass | time.query | disk/q3 | 4.63 | 5.07 | 1.07 | 3.00 |  |
| pass | time.query | disk/q4 | 2.12 | 2.23 | 1.03 | 3.00 |  |
| pass | time.query | disk/q5 | 264.42 | 251.55 | 0.95 | 3.00 |  |
| pass | time.query | disk/q6 | 163.68 | 223.48 | 1.36 | 3.00 |  |
| pass | time.query | disk/q7 | 358.48 | 359.09 | 1.00 | 3.00 |  |
| pass | time.query | disk/q8 | 371.78 | 319.57 | 0.86 | 3.00 |  |
| pass | time.query | disk/q9 | 490.84 | 612.70 | 1.25 | 3.00 |  |
| pass | time.query | disk/q10 | 278.67 | 273.52 | 0.98 | 3.00 |  |
| pass | time.query | disk/q11 | 0.96 | 1.38 | 1.14 | 3.00 |  |
| pass | time.query | disk/q12 | 0.55 | 0.69 | 1.05 | 3.00 |  |
| pass | time.query | disk/q13 | 238.14 | 293.77 | 1.23 | 3.00 |  |
| pass | time.query | disk/q14 | 118.82 | 159.33 | 1.34 | 3.00 |  |
| pass | time.geomean | disk | – | – | 1.12 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | disk | 21012.10 | 20798.94 | 0.99 | 2.50 |  |
| pass | time.open | disk | 1407.22 | 0.65 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | disk | 460.73 | 210.85 | 0.49 | 1.50 |  |
| pass | rss.build_peak | disk | 434.44 | 428.72 | 0.99 | 1.50 |  |
| pass | rss.after_open | disk | 350.69 | 5.82 | 0.10 | 1.50 |  |
| pass | bytes.dir | disk | 653959173.00 | 653959173.00 | 1.00 | 1.25 |  |
| pass | info.conc_qps | disk/sparql/q1/c1 | 195.64 | 194.67 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 444.25 | 430.16 | 0.97 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 381.48 | 379.83 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 920.72 | 953.54 | 1.04 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 200.75 | 185.24 | 0.92 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 416.70 | 443.83 | 1.07 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 355.51 | 355.25 | 1.00 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 908.26 | 934.31 | 1.03 | – | report-only |

exit: 0

# Stream-mode disk against the pre-Phase-6 disk layout (pre-phase6-lubm38.json, --backends disk)

## gstore-bench: reference profile, lubm-like univ 38 (5181103 unique triples)

- generator `lubm-like/1`, data fnv `fcd6493725f3c66a`, 5363690 lines, 949.6 MB
- commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.17 1.08 1.51
- calibration score 59.27 ms (sort 44.92, hashmap 101.58, format 45.63, fsync 0.33)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| disk | 20.06 | 428.7 | 654.0 | 10.5 (cold) | 0.6 | 5.8 | 210.8 | 44.13 | 0.30× |

| query | rows | disk first ms | disk median ms | disk vs base |
|---|---:|---:|---:|---:|
| q1 | 3 | 10.19 | 4.14 | 0.11× |
| q2 | 115 | 81.49 | 72.75 | 0.38× |
| q3 | 10 | 11.86 | 4.89 | 0.12× |
| q4 | 9 | 2.82 | 2.15 | 0.03× |
| q5 | 576 | 311.40 | 242.66 | 0.39× |
| q6 | 404399 | 327.62 | 215.59 | 0.42× |
| q7 | 23 | 363.50 | 346.40 | 0.30× |
| q8 | 10778 | 304.91 | 308.28 | 0.35× |
| q9 | 5507 | 566.43 | 591.06 | 0.41× |
| q10 | 3 | 258.19 | 263.86 | 0.38× |
| q11 | 325 | 3.64 | 1.33 | 0.89× |
| q12 | 21 | 2.29 | 0.66 | 0.92× |
| q13 | 432512 | 303.21 | 283.39 | 0.31× |
| q14 | 305503 | 149.00 | 153.70 | 0.48× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| disk | 1.05 / 1.82 | 0.88 / 1.16 | 6478 | 893.4 | 934.6 | 0.0 | 0.7 | 842657 → 843783 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| disk | sparql | q1 | 1 | 194.7 | 5.13 | 6.97 | 0 | identical | 8.34× |
| disk | sparql | q1 | 8 | 430.2 | 18.60 | 27.57 | 0 | identical | 19.14× |
| disk | sparql | q4 | 1 | 379.8 | 2.63 | 3.47 | 0 | identical | 30.15× |
| disk | sparql | q4 | 8 | 953.5 | 8.38 | 10.89 | 0 | identical | 81.59× |
| disk | api | q1 | 1 | 185.2 | 5.38 | 7.33 | 0 | identical | 7.95× |
| disk | api | q1 | 8 | 443.8 | 18.00 | 26.05 | 0 | identical | 20.01× |
| disk | api | q4 | 1 | 355.2 | 2.76 | 3.95 | 0 | identical | 30.51× |
| disk | api | q4 | 8 | 934.3 | 8.50 | 10.91 | 0 | identical | 81.44× |

### Gates: PASS (105 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | disk/q1 | 39.06 | 4.38 | 0.16 | 3.00 |  |
| pass | time.query | disk/q2 | 189.65 | 76.82 | 0.41 | 3.00 |  |
| pass | time.query | disk/q3 | 39.17 | 5.16 | 0.17 | 3.00 |  |
| pass | time.query | disk/q4 | 75.19 | 2.27 | 0.06 | 3.00 |  |
| pass | time.query | disk/q5 | 621.34 | 256.23 | 0.41 | 3.00 |  |
| pass | time.query | disk/q6 | 511.38 | 227.64 | 0.45 | 3.00 |  |
| pass | time.query | disk/q7 | 1147.71 | 365.76 | 0.32 | 3.00 |  |
| pass | time.query | disk/q8 | 871.82 | 325.51 | 0.37 | 3.00 |  |
| pass | time.query | disk/q9 | 1450.56 | 624.09 | 0.43 | 3.00 |  |
| pass | time.query | disk/q10 | 697.13 | 278.61 | 0.40 | 3.00 |  |
| pass | time.query | disk/q11 | 1.49 | 1.40 | 0.98 | 3.00 |  |
| pass | time.query | disk/q12 | 0.72 | 0.70 | 0.99 | 3.00 |  |
| pass | time.query | disk/q13 | 906.09 | 299.23 | 0.33 | 3.00 |  |
| pass | time.query | disk/q14 | 320.35 | 162.29 | 0.51 | 3.00 |  |
| pass | time.geomean | disk | – | – | 0.29 | 1.50 | geomean of 13 warm-median ratios |
| pass | time.build | disk | 1627585.07 | 21185.58 | 0.01 | 2.50 |  |
| pass | time.open | disk | 7673.50 | 0.66 | 0.00 | 2.50 |  |
| pass | rss.query_hwm | disk | 740.63 | 210.85 | 0.31 | 1.50 |  |
| pass | rss.build_peak | disk | 738.11 | 428.72 | 0.60 | 1.50 |  |
| pass | rss.after_open | disk | 543.97 | 5.82 | 0.07 | 1.50 |  |
| pass | bytes.dir | disk | 889778182.00 | 653959173.00 | 0.73 | 1.25 |  |
| pass | info.conc_qps | disk/sparql/q1/c1 | 23.33 | 194.67 | 8.34 | – | report-only |
| pass | info.conc_qps | disk/sparql/q1/c8 | 22.48 | 430.16 | 19.14 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c1 | 12.60 | 379.83 | 30.15 | – | report-only |
| pass | info.conc_qps | disk/sparql/q4/c8 | 11.69 | 953.54 | 81.59 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c1 | 23.30 | 185.24 | 7.95 | – | report-only |
| pass | info.conc_qps | disk/api/q1/c8 | 22.18 | 443.83 | 20.01 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c1 | 11.64 | 355.25 | 30.51 | – | report-only |
| pass | info.conc_qps | disk/api/q4/c8 | 11.47 | 934.31 | 81.44 | – | report-only |

exit: 0
