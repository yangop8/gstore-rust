## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.06 0.34 0.62
- calibration score 70.47 ms (sort 43.81, hashmap 191.69, format 41.68, fsync 0.50)
- compared with: reference profile, commit `4ef114181db4a98757513bd0f7437ddf94c8bd38`

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.41 | 112.8 | 85.5 | 233.6 (cold) | 202.6 | 102.2 | 128.4 | 2.16 | 1.02× | 1.08× | 0.69× |
| disk@materialize | 3.55 | 108.4 | 130.2 | 305.8 (cold) | 267.2 | 89.1 | 115.5 | 2.05 | 0.79× | 1.03× | 0.37× |
| disk@stream | 3.65 | 96.9 | 130.2 | 4.1 (cold) | 0.8 | 6.0 | 66.0 | 3.06 | 0.81× | 0.00× | 0.56× |
| rocksdb | 4.31 | 92.6 | 31.1 | 164.4 (cold) | 144.1 | 66.4 | 102.0 | 5.16 | 1.02× | 0.78× | 0.49× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 3101, spo 134, index 209, pos 127, osp+vstree 209, dictionary 60, finish 91, install 4 | 1024.0 | 88.0 | 24.1 | 4 | 0 | 0.0 |
| disk@stream | parse+encode 3209, spo 122, index 199, pos 88, osp+vstree 198, dictionary 79, finish 106, install 2 | 1024.0 | 80.0 | 24.1 | 4 | 0 | 0.0 |
| rocksdb | parse+encode 3533, spo 392, index 284, pos 165, osp 202, x-counts 124, dictionary 59, ingest 76, install 1 | 1024.0 | 60.0 | 24.1 | 4 | 0 | 0.0 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.17 | 0.01 | 0.36× | 0.18 | 0.01 | 0.03× | 0.31 | 0.02 | 0.05× | 0.35 | 0.04 | 0.02× |
| q2 | 25 | 6.87 | 5.53 | 0.92× | 5.45 | 4.51 | 0.54× | 14.94 | 6.98 | 0.84× | 9.25 | 5.49 | 0.95× |
| q3 | 10 | 0.06 | 0.01 | 0.19× | 0.06 | 0.01 | 0.03× | 0.11 | 0.02 | 0.04× | 0.18 | 0.05 | 0.01× |
| q4 | 9 | 0.09 | 0.05 | 0.70× | 0.10 | 0.05 | 0.29× | 0.15 | 0.07 | 0.42× | 0.21 | 0.17 | 0.36× |
| q5 | 576 | 29.30 | 21.67 | 0.88× | 31.19 | 24.43 | 0.75× | 34.97 | 28.93 | 0.88× | 53.17 | 43.36 | 1.12× |
| q6 | 80056 | 13.96 | 13.85 | 0.63× | 15.82 | 16.53 | 0.58× | 50.43 | 32.47 | 1.14× | 37.89 | 34.32 | 0.87× |
| q7 | 23 | 27.31 | 29.05 | 0.95× | 28.91 | 29.05 | 0.64× | 38.51 | 29.31 | 0.65× | 48.74 | 50.95 | 1.10× |
| q8 | 10778 | 32.89 | 32.88 | 0.96× | 30.10 | 31.76 | 0.74× | 50.95 | 45.77 | 1.06× | 92.81 | 93.63 | 1.01× |
| q9 | 1081 | 42.40 | 43.59 | 0.98× | 49.13 | 47.90 | 0.75× | 81.28 | 65.06 | 1.02× | 117.85 | 103.63 | 0.97× |
| q10 | 3 | 22.08 | 21.52 | 0.95× | 22.56 | 23.43 | 0.78× | 30.29 | 28.81 | 0.95× | 43.29 | 45.17 | 1.18× |
| q11 | 325 | 0.60 | 0.23 | 0.96× | 0.37 | 0.13 | 0.42× | 0.78 | 0.21 | 0.65× | 2.08 | 0.92 | 0.88× |
| q12 | 21 | 0.31 | 0.10 | 0.76× | 0.10 | 0.05 | 0.38× | 0.37 | 0.09 | 0.71× | 0.31 | 0.17 | 0.88× |
| q13 | 85670 | 15.56 | 15.45 | 0.63× | 19.96 | 20.14 | 0.53× | 40.82 | 35.95 | 0.94× | 39.61 | 38.17 | 0.90× |
| q14 | 60576 | 10.32 | 10.14 | 0.63× | 12.35 | 10.95 | 0.55× | 26.13 | 25.10 | 1.27× | 26.98 | 27.20 | 0.89× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.17 | 0.01 / 0.01 | 5330 | 40.1 | 27.2 | 307.1 | 147.5 | 168432 → 168432 | ok |
| disk@materialize | 1.51 / 2.49 | 1.67 / 3.12 | 5330 | 659.1 | 1014.1 | 0.1 | 558.3 | 168432 → 168432 | ok |
| disk@stream | 2.01 / 3.45 | 2.71 / 4.32 | 5330 | 562.4 | 1000.7 | 0.1 | 0.8 | 168432 → 168432 | ok |
| rocksdb | 0.27 / 0.56 | 0.27 / 0.50 | 5330 | 123.5 | 106.2 | 34.7 | 132.1 | 168432 → 168432 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 12532.2 | 0.08 | 0.12 | 0 | identical | 1.23× |
| snapshot | sparql | q1 | 8 | 69790.8 | 0.11 | 0.29 | 0 | identical | 1.26× |
| snapshot | sparql | q4 | 1 | 5833.9 | 0.17 | 0.30 | 0 | identical | 1.16× |
| snapshot | sparql | q4 | 8 | 30561.4 | 0.26 | 0.56 | 0 | identical | 1.12× |
| snapshot | api | q1 | 1 | 5410.5 | 0.17 | 0.33 | 0 | identical | 1.07× |
| snapshot | api | q1 | 8 | 13856.1 | 0.56 | 1.29 | 0 | identical | 0.94× |
| snapshot | api | q4 | 1 | 2559.6 | 0.34 | 0.59 | 0 | identical | 0.90× |
| snapshot | api | q4 | 8 | 9957.8 | 0.75 | 1.64 | 0 | identical | 0.87× |
| disk@materialize | sparql | q1 | 1 | 11552.9 | 0.09 | 0.13 | 0 | identical | 6.01× |
| disk@materialize | sparql | q1 | 8 | 61370.2 | 0.12 | 0.34 | 0 | identical | 13.06× |
| disk@materialize | sparql | q4 | 1 | 5661.0 | 0.17 | 0.29 | 0 | identical | 2.15× |
| disk@materialize | sparql | q4 | 8 | 31466.2 | 0.25 | 0.56 | 0 | identical | 3.83× |
| disk@materialize | api | q1 | 1 | 5443.3 | 0.17 | 0.34 | 0 | identical | 3.66× |
| disk@materialize | api | q1 | 8 | 15633.8 | 0.50 | 1.13 | 0 | identical | 3.71× |
| disk@materialize | api | q4 | 1 | 2508.1 | 0.36 | 0.63 | 0 | identical | 1.52× |
| disk@materialize | api | q4 | 8 | 11465.6 | 0.65 | 1.41 | 0 | identical | 1.90× |
| disk@stream | sparql | q1 | 1 | 10774.6 | 0.09 | 0.15 | 0 | identical | 5.60× |
| disk@stream | sparql | q1 | 8 | 45735.6 | 0.17 | 0.40 | 0 | identical | 9.73× |
| disk@stream | sparql | q4 | 1 | 4660.2 | 0.21 | 0.37 | 0 | identical | 1.77× |
| disk@stream | sparql | q4 | 8 | 20254.9 | 0.39 | 0.84 | 0 | identical | 2.47× |
| disk@stream | api | q1 | 1 | 4604.4 | 0.21 | 0.40 | 0 | identical | 3.10× |
| disk@stream | api | q1 | 8 | 15193.7 | 0.51 | 1.16 | 0 | identical | 3.61× |
| disk@stream | api | q4 | 1 | 2017.7 | 0.45 | 0.74 | 0 | identical | 1.22× |
| disk@stream | api | q4 | 8 | 8832.3 | 0.85 | 1.86 | 0 | identical | 1.46× |
| rocksdb | sparql | q1 | 1 | 8800.4 | 0.11 | 0.18 | 0 | identical | – |
| rocksdb | sparql | q1 | 8 | 49224.8 | 0.16 | 0.39 | 0 | identical | – |
| rocksdb | sparql | q4 | 1 | 2502.6 | 0.40 | 0.67 | 0 | identical | – |
| rocksdb | sparql | q4 | 8 | 10208.7 | 0.78 | 1.68 | 0 | identical | – |
| rocksdb | api | q1 | 1 | 4117.5 | 0.23 | 0.46 | 0 | identical | – |
| rocksdb | api | q1 | 8 | 11462.2 | 0.68 | 1.56 | 0 | identical | – |
| rocksdb | api | q4 | 1 | 1383.1 | 0.65 | 1.05 | 0 | identical | – |
| rocksdb | api | q4 | 8 | 6296.1 | 1.22 | 2.65 | 0 | identical | – |

| backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | off | q4 | 8 | 29775.2 | 0.26 | 0.58 | 51 / 0 | 0.71 / 2.08 | identical | – |
| snapshot | sparql | on | q4 | 8 | 42.2 | 189.63 | 530.05 | 18 / 0 | 306.00 / 543.56 | identical | – |
| snapshot | api | off | q4 | 8 | 10264.8 | 0.71 | 1.53 | 51 / 0 | 1.23 / 2.18 | identical | – |
| snapshot | api | on | q4 | 8 | 38.1 | 209.59 | 280.89 | 21 / 0 | 261.72 / 280.47 | identical | – |
| disk@materialize | sparql | off | q4 | 8 | 28645.6 | 0.27 | 0.59 | 51 / 0 | 3.72 / 5.78 | identical | – |
| disk@materialize | sparql | on | q4 | 8 | 29568.8 | 0.26 | 0.58 | 51 / 0 | 4.06 / 7.11 | identical | – |
| disk@materialize | api | off | q4 | 8 | 9688.9 | 0.76 | 1.80 | 51 / 0 | 4.42 / 6.11 | identical | – |
| disk@materialize | api | on | q4 | 8 | 8942.2 | 0.82 | 1.86 | 51 / 0 | 4.53 / 6.96 | identical | – |
| disk@stream | sparql | off | q4 | 8 | 20557.6 | 0.38 | 0.85 | 51 / 0 | 3.90 / 5.64 | identical | – |
| disk@stream | sparql | on | q4 | 8 | 20972.9 | 0.37 | 0.82 | 51 / 0 | 3.62 / 5.38 | identical | – |
| disk@stream | api | off | q4 | 8 | 7644.0 | 0.97 | 2.04 | 51 / 0 | 11.15 / 22.94 | identical | – |
| disk@stream | api | on | q4 | 8 | 8064.2 | 0.91 | 2.03 | 51 / 0 | 4.63 / 6.64 | identical | – |
| rocksdb | sparql | off | q4 | 8 | 8579.1 | 0.92 | 2.09 | 51 / 0 | 2.47 / 3.93 | identical | – |
| rocksdb | sparql | on | q4 | 8 | 4420.6 | 1.80 | 4.11 | 51 / 0 | 16.36 / 25.17 | identical | – |
| rocksdb | api | off | q4 | 8 | 6261.0 | 1.19 | 2.46 | 51 / 0 | 3.12 / 5.25 | identical | – |
| rocksdb | api | on | q4 | 8 | 3486.2 | 2.20 | 4.98 | 51 / 0 | 16.50 / 28.74 | identical | – |

| backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB | median vs base |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 20 | 0.44 | 1.03 | 1.19 | 0.0 / 0.0 | – |
| disk@materialize | 20 | 0.46 | 1.22 | 1.52 | 0.0 / 0.0 | – |
| disk@stream | 20 | 0.40 | 0.83 | 1.02 | 0.1 / 0.1 | – |
| rocksdb | 20 | 311.25 | 374.85 | 578.83 | 53.8 / 53.8 | – |

### Gates: PASS (365 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.03 | 0.01 | 0.99 | 3.00 |  |
| pass | time.query | snapshot/q2 | 6.03 | 4.42 | 0.80 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.07 | 0.01 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q4 | 0.07 | 0.04 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q5 | 24.74 | 17.32 | 0.72 | 3.00 |  |
| pass | time.query | snapshot/q6 | 21.93 | 11.07 | 0.55 | 3.00 |  |
| pass | time.query | snapshot/q7 | 30.47 | 23.22 | 0.78 | 3.00 |  |
| pass | time.query | snapshot/q8 | 34.11 | 26.28 | 0.78 | 3.00 |  |
| pass | time.query | snapshot/q9 | 44.27 | 34.85 | 0.80 | 3.00 |  |
| pass | time.query | snapshot/q10 | 22.64 | 17.20 | 0.78 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.24 | 0.19 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.13 | 0.08 | 0.98 | 3.00 |  |
| pass | time.query | snapshot/q13 | 24.45 | 12.35 | 0.54 | 3.00 |  |
| pass | time.query | snapshot/q14 | 16.10 | 8.11 | 0.56 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.66 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | snapshot | 5308.61 | 4321.55 | 0.82 | 2.00 |  |
| pass | time.open | snapshot | 187.10 | 161.96 | 0.88 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 0.36 | 0.01 | 0.85 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 8.36 | 3.26 | 0.51 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 0.41 | 0.01 | 0.83 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 0.18 | 0.04 | 0.94 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 32.72 | 17.65 | 0.57 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 28.36 | 11.94 | 0.46 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 45.21 | 20.99 | 0.49 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 43.08 | 22.95 | 0.55 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 64.05 | 34.61 | 0.55 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 30.22 | 16.93 | 0.59 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 0.32 | 0.10 | 0.90 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 0.13 | 0.03 | 0.96 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 38.05 | 14.55 | 0.41 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 19.75 | 7.91 | 0.46 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.46 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk@materialize | 4493.37 | 2563.61 | 0.58 | 2.50 |  |
| pass | time.open | disk@materialize | 259.05 | 193.07 | 0.76 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 0.36 | 0.01 | 0.85 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 8.36 | 5.59 | 0.73 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 0.41 | 0.01 | 0.84 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 0.18 | 0.06 | 0.95 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 32.72 | 23.16 | 0.72 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 28.36 | 25.99 | 0.92 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 45.21 | 23.46 | 0.54 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 43.08 | 36.64 | 0.86 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 64.05 | 52.09 | 0.82 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 30.22 | 23.06 | 0.78 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 0.32 | 0.17 | 0.93 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.13 | 0.07 | 0.97 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 38.05 | 28.78 | 0.77 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 19.75 | 20.09 | 1.02 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.77 | 1.50 | geomean of 9 warm-median ratios |
| pass | time.build | disk@stream | 4493.37 | 2919.00 | 0.66 | 2.50 |  |
| pass | time.open | disk@stream | 259.05 | 0.65 | 0.07 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 1.89 | 0.03 | 0.52 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 5.80 | 4.57 | 0.84 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 6.02 | 0.04 | 0.25 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 0.47 | 0.14 | 0.87 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 38.84 | 36.07 | 0.93 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 39.45 | 28.55 | 0.74 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 46.14 | 42.39 | 0.92 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 93.06 | 77.89 | 0.84 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 106.81 | 86.21 | 0.81 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 38.30 | 37.58 | 0.98 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 1.04 | 0.77 | 0.91 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 0.19 | 0.14 | 0.98 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 42.35 | 31.76 | 0.76 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 30.65 | 22.62 | 0.75 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.40 | 1.50 | geomean of 12 warm-median ratios |
| pass | time.build | rocksdb | 4212.13 | 3583.11 | 0.85 | 2.00 |  |
| pass | time.open | rocksdb | 184.35 | 119.87 | 0.68 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 128.79 | 128.36 | 1.00 | 1.50 |  |
| pass | rss.build_peak | snapshot | 112.67 | 112.77 | 1.00 | 1.50 |  |
| pass | rss.after_open | snapshot | 101.97 | 102.23 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 119.25 | 115.48 | 0.98 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 98.72 | 108.43 | 1.07 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 88.63 | 89.09 | 1.00 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 119.25 | 65.95 | 0.65 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 98.72 | 96.93 | 0.99 | 1.50 |  |
| pass | rss.after_open | disk@stream | 88.63 | 6.05 | 0.32 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 102.61 | 101.99 | 1.00 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 93.36 | 92.64 | 0.99 | 1.50 |  |
| pass | rss.after_open | rocksdb | 68.27 | 66.41 | 0.98 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 130150405.00 | 130150400.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@stream | 130150405.00 | 130150400.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | rocksdb | 31266467.00 | 31059804.00 | 0.99 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 10173.24 | 12532.20 | 1.23 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 55390.72 | 69790.78 | 1.26 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 5038.13 | 5833.94 | 1.16 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 27252.85 | 30561.39 | 1.12 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 5034.70 | 5410.49 | 1.07 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 14788.13 | 13856.12 | 0.94 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 2852.46 | 2559.64 | 0.90 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 11469.60 | 9957.75 | 0.87 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 1923.50 | 11552.87 | 6.01 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 4698.40 | 61370.21 | 13.06 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 2629.36 | 5661.00 | 2.15 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 8206.31 | 31466.18 | 3.83 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 1485.28 | 5443.28 | 3.66 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 4212.98 | 15633.83 | 3.71 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 1652.34 | 2508.12 | 1.52 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 6046.12 | 11465.57 | 1.90 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 1923.50 | 10774.60 | 5.60 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 4698.40 | 45735.64 | 9.73 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 2629.36 | 4660.19 | 1.77 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 8206.31 | 20254.93 | 2.47 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 1485.28 | 4604.40 | 3.10 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 4212.98 | 15193.69 | 3.61 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 1652.34 | 2017.69 | 1.22 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 6046.12 | 8832.27 | 1.46 | – | report-only |

