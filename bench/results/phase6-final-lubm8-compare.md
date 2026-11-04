## gstore-bench: reference profile, lubm-like univ 8 (1029275 unique triples)

- commit `769c62797125abaa28dfc1b40dcb8bb138ac278e`, linux x86_64 × 4 CPUs, 16091 MiB, load 0.06 0.34 0.62
- calibration score 70.47 ms (sort 43.81, hashmap 191.69, format 41.68, fsync 0.50)
- compared with: reference profile, commit `e859f7eb48cf0307bfe8337992294a76ff48b8d6`

- generator `lubm-like/1`, data fnv `ed44da2e7b3bbb98`, 1064650 lines, 187.3 MB

| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 5.41 | 112.8 | 85.5 | 233.6 (cold) | 202.6 | 102.2 | 128.4 | 2.16 | 0.35× | 0.69× | 0.27× |
| disk@materialize | 3.55 | 108.4 | 130.2 | 305.8 (cold) | 267.2 | 89.1 | 115.5 | 2.05 | 0.01× | 0.18× | 0.11× |
| disk@stream | 3.65 | 96.9 | 130.2 | 4.1 (cold) | 0.8 | 6.0 | 66.0 | 3.06 | 0.01× | 0.00× | 0.16× |
| rocksdb | 4.31 | 92.6 | 31.1 | 164.4 (cold) | 144.1 | 66.4 | 102.0 | 5.16 | 0.08× | 0.01× | 0.12× |

| backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---:|---:|---:|---:|---:|---:|
| disk@materialize | parse+encode 3101, spo 134, index 209, pos 127, osp+vstree 209, dictionary 60, finish 91, install 4 | 1024.0 | 88.0 | 24.1 | 4 | 0 | 0.0 |
| disk@stream | parse+encode 3209, spo 122, index 199, pos 88, osp+vstree 198, dictionary 79, finish 106, install 2 | 1024.0 | 80.0 | 24.1 | 4 | 0 | 0.0 |
| rocksdb | parse+encode 3533, spo 392, index 284, pos 165, osp 202, x-counts 124, dictionary 59, ingest 76, install 1 | 1024.0 | 60.0 | 24.1 | 4 | 0 | 0.0 |

| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.17 | 0.01 | 0.04× | 0.18 | 0.01 | 0.03× | 0.31 | 0.02 | 0.04× | 0.35 | 0.04 | 0.00× |
| q2 | 25 | 6.87 | 5.53 | 0.69× | 5.45 | 4.51 | 0.12× | 14.94 | 6.98 | 0.18× | 9.25 | 5.49 | 0.06× |
| q3 | 10 | 0.06 | 0.01 | 0.03× | 0.06 | 0.01 | 0.03× | 0.11 | 0.02 | 0.04× | 0.18 | 0.05 | 0.00× |
| q4 | 9 | 0.09 | 0.05 | 0.01× | 0.10 | 0.05 | 0.01× | 0.15 | 0.07 | 0.01× | 0.21 | 0.17 | 0.00× |
| q5 | 576 | 29.30 | 21.67 | 0.58× | 31.19 | 24.43 | 0.21× | 34.97 | 28.93 | 0.25× | 53.17 | 43.36 | 0.62× |
| q6 | 80056 | 13.96 | 13.85 | 0.45× | 15.82 | 16.53 | 0.17× | 50.43 | 32.47 | 0.33× | 37.89 | 34.32 | 0.50× |
| q7 | 23 | 27.31 | 29.05 | 0.54× | 28.91 | 29.05 | 0.13× | 38.51 | 29.31 | 0.14× | 48.74 | 50.95 | 0.39× |
| q8 | 10778 | 32.89 | 32.88 | 0.64× | 30.10 | 31.76 | 0.19× | 50.95 | 45.77 | 0.28× | 92.81 | 93.63 | 0.58× |
| q9 | 1081 | 42.40 | 43.59 | 0.62× | 49.13 | 47.90 | 0.18× | 81.28 | 65.06 | 0.24× | 117.85 | 103.63 | 0.43× |
| q10 | 3 | 22.08 | 21.52 | 0.60× | 22.56 | 23.43 | 0.21× | 30.29 | 28.81 | 0.25× | 43.29 | 45.17 | 0.65× |
| q11 | 325 | 0.60 | 0.23 | 0.60× | 0.37 | 0.13 | 0.25× | 0.78 | 0.21 | 0.38× | 2.08 | 0.92 | 0.34× |
| q12 | 21 | 0.31 | 0.10 | 0.65× | 0.10 | 0.05 | 0.31× | 0.37 | 0.09 | 0.58× | 0.31 | 0.17 | 0.11× |
| q13 | 85670 | 15.56 | 15.45 | 0.44× | 19.96 | 20.14 | 0.12× | 40.82 | 35.95 | 0.21× | 39.61 | 38.17 | 0.50× |
| q14 | 60576 | 10.32 | 10.14 | 0.48× | 12.35 | 10.95 | 0.18× | 26.13 | 25.10 | 0.42× | 26.98 | 27.20 | 0.54× |

| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 0.06 / 0.17 | 0.01 / 0.01 | 5330 | 40.1 | 27.2 | 307.1 | 147.5 | 168432 → 168432 | ok |
| disk@materialize | 1.51 / 2.49 | 1.67 / 3.12 | 5330 | 659.1 | 1014.1 | 0.1 | 558.3 | 168432 → 168432 | ok |
| disk@stream | 2.01 / 3.45 | 2.71 / 4.32 | 5330 | 562.4 | 1000.7 | 0.1 | 0.8 | 168432 → 168432 | ok |
| rocksdb | 0.27 / 0.56 | 0.27 / 0.50 | 5330 | 123.5 | 106.2 | 34.7 | 132.1 | 168432 → 168432 | ok |

| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---:|---:|---:|---:|---:|---|---:|
| snapshot | sparql | q1 | 1 | 12532.2 | 0.08 | 0.12 | 0 | identical | 6.26× |
| snapshot | sparql | q1 | 8 | 69790.8 | 0.11 | 0.29 | 0 | identical | 33.24× |
| snapshot | sparql | q4 | 1 | 5833.9 | 0.17 | 0.30 | 0 | identical | 72.00× |
| snapshot | sparql | q4 | 8 | 30561.4 | 0.26 | 0.56 | 0 | identical | 400.37× |
| snapshot | api | q1 | 1 | 5410.5 | 0.17 | 0.33 | 0 | identical | 3.35× |
| snapshot | api | q1 | 8 | 13856.1 | 0.56 | 1.29 | 0 | identical | 6.04× |
| snapshot | api | q4 | 1 | 2559.6 | 0.34 | 0.59 | 0 | identical | 29.89× |
| snapshot | api | q4 | 8 | 9957.8 | 0.75 | 1.64 | 0 | identical | 130.12× |
| disk@materialize | sparql | q1 | 1 | 11552.9 | 0.09 | 0.13 | 0 | identical | 7.49× |
| disk@materialize | sparql | q1 | 8 | 61370.2 | 0.12 | 0.34 | 0 | identical | 42.34× |
| disk@materialize | sparql | q4 | 1 | 5661.0 | 0.17 | 0.29 | 0 | identical | 66.70× |
| disk@materialize | sparql | q4 | 8 | 31466.2 | 0.25 | 0.56 | 0 | identical | 396.87× |
| disk@materialize | api | q1 | 1 | 5443.3 | 0.17 | 0.34 | 0 | identical | 4.49× |
| disk@materialize | api | q1 | 8 | 15633.8 | 0.50 | 1.13 | 0 | identical | 10.70× |
| disk@materialize | api | q4 | 1 | 2508.1 | 0.36 | 0.63 | 0 | identical | 29.21× |
| disk@materialize | api | q4 | 8 | 11465.6 | 0.65 | 1.41 | 0 | identical | 150.00× |
| disk@stream | sparql | q1 | 1 | 10774.6 | 0.09 | 0.15 | 0 | identical | 6.98× |
| disk@stream | sparql | q1 | 8 | 45735.6 | 0.17 | 0.40 | 0 | identical | 31.55× |
| disk@stream | sparql | q4 | 1 | 4660.2 | 0.21 | 0.37 | 0 | identical | 54.91× |
| disk@stream | sparql | q4 | 8 | 20254.9 | 0.39 | 0.84 | 0 | identical | 255.46× |
| disk@stream | api | q1 | 1 | 4604.4 | 0.21 | 0.40 | 0 | identical | 3.80× |
| disk@stream | api | q1 | 8 | 15193.7 | 0.51 | 1.16 | 0 | identical | 10.40× |
| disk@stream | api | q4 | 1 | 2017.7 | 0.45 | 0.74 | 0 | identical | 23.50× |
| disk@stream | api | q4 | 8 | 8832.3 | 0.85 | 1.86 | 0 | identical | 115.55× |
| rocksdb | sparql | q1 | 1 | 8800.4 | 0.11 | 0.18 | 0 | identical | 109.66× |
| rocksdb | sparql | q1 | 8 | 49224.8 | 0.16 | 0.39 | 0 | identical | 653.50× |
| rocksdb | sparql | q4 | 1 | 2502.6 | 0.40 | 0.67 | 0 | identical | 349.01× |
| rocksdb | sparql | q4 | 8 | 10208.7 | 0.78 | 1.68 | 0 | identical | 1621.28× |
| rocksdb | api | q1 | 1 | 4117.5 | 0.23 | 0.46 | 0 | identical | 44.01× |
| rocksdb | api | q1 | 8 | 11462.2 | 0.68 | 1.56 | 0 | identical | 147.15× |
| rocksdb | api | q4 | 1 | 1383.1 | 0.65 | 1.05 | 0 | identical | 185.71× |
| rocksdb | api | q4 | 8 | 6296.1 | 1.22 | 2.65 | 0 | identical | 942.56× |

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

### Gates: PASS (373 checked, 0 failed)

| result | gate | subject | base | measured | ratio | limit | detail |
|---|---|---|---:|---:|---:|---:|---|
| pass | time.query | snapshot/q1 | 0.31 | 0.01 | 0.87 | 3.00 |  |
| pass | time.query | snapshot/q2 | 8.04 | 5.29 | 0.73 | 3.00 |  |
| pass | time.query | snapshot/q3 | 0.44 | 0.01 | 0.83 | 3.00 |  |
| pass | time.query | snapshot/q4 | 9.50 | 0.05 | 0.18 | 3.00 |  |
| pass | time.query | snapshot/q5 | 37.32 | 20.73 | 0.58 | 3.00 |  |
| pass | time.query | snapshot/q6 | 31.02 | 13.25 | 0.46 | 3.00 |  |
| pass | time.query | snapshot/q7 | 54.04 | 27.79 | 0.53 | 3.00 |  |
| pass | time.query | snapshot/q8 | 51.02 | 31.45 | 0.63 | 3.00 |  |
| pass | time.query | snapshot/q9 | 70.37 | 41.71 | 0.60 | 3.00 |  |
| pass | time.query | snapshot/q10 | 36.11 | 20.59 | 0.59 | 3.00 |  |
| pass | time.query | snapshot/q11 | 0.39 | 0.22 | 0.93 | 3.00 |  |
| pass | time.query | snapshot/q12 | 0.16 | 0.10 | 0.97 | 3.00 |  |
| pass | time.query | snapshot/q13 | 35.21 | 14.78 | 0.45 | 3.00 |  |
| pass | time.query | snapshot/q14 | 21.10 | 9.70 | 0.51 | 3.00 |  |
| pass | time.geomean | snapshot | – | – | 0.33 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | snapshot | 15315.60 | 5172.05 | 0.34 | 2.00 |  |
| pass | time.open | snapshot | 292.10 | 193.84 | 0.69 | 2.50 |  |
| pass | time.query | disk@materialize/q1 | 0.42 | 0.01 | 0.83 | 3.00 |  |
| pass | time.query | disk@materialize/q2 | 38.39 | 3.82 | 0.14 | 3.00 |  |
| pass | time.query | disk@materialize/q3 | 0.48 | 0.01 | 0.81 | 3.00 |  |
| pass | time.query | disk@materialize/q4 | 9.73 | 0.04 | 0.17 | 3.00 |  |
| pass | time.query | disk@materialize/q5 | 115.47 | 20.72 | 0.19 | 3.00 |  |
| pass | time.query | disk@materialize/q6 | 97.27 | 14.02 | 0.16 | 3.00 |  |
| pass | time.query | disk@materialize/q7 | 216.50 | 24.64 | 0.12 | 3.00 |  |
| pass | time.query | disk@materialize/q8 | 164.92 | 26.94 | 0.17 | 3.00 |  |
| pass | time.query | disk@materialize/q9 | 266.26 | 40.63 | 0.16 | 3.00 |  |
| pass | time.query | disk@materialize/q10 | 114.01 | 19.87 | 0.19 | 3.00 |  |
| pass | time.query | disk@materialize/q11 | 0.54 | 0.11 | 0.83 | 3.00 |  |
| pass | time.query | disk@materialize/q12 | 0.15 | 0.04 | 0.95 | 3.00 |  |
| pass | time.query | disk@materialize/q13 | 172.05 | 17.09 | 0.11 | 3.00 |  |
| pass | time.query | disk@materialize/q14 | 60.17 | 9.29 | 0.18 | 3.00 |  |
| pass | time.geomean | disk@materialize | – | – | 0.10 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk@materialize | 263743.67 | 3009.81 | 0.01 | 2.50 |  |
| pass | time.open | disk@materialize | 1447.34 | 226.68 | 0.17 | 2.50 |  |
| pass | time.query | disk@stream/q1 | 0.42 | 0.02 | 0.83 | 3.00 |  |
| pass | time.query | disk@stream/q2 | 38.39 | 6.56 | 0.21 | 3.00 |  |
| pass | time.query | disk@stream/q3 | 0.48 | 0.02 | 0.81 | 3.00 |  |
| pass | time.query | disk@stream/q4 | 9.73 | 0.07 | 0.18 | 3.00 |  |
| pass | time.query | disk@stream/q5 | 115.47 | 27.20 | 0.25 | 3.00 |  |
| pass | time.query | disk@stream/q6 | 97.27 | 30.52 | 0.33 | 3.00 |  |
| pass | time.query | disk@stream/q7 | 216.50 | 27.55 | 0.14 | 3.00 |  |
| pass | time.query | disk@stream/q8 | 164.92 | 43.02 | 0.27 | 3.00 |  |
| pass | time.query | disk@stream/q9 | 266.26 | 61.15 | 0.24 | 3.00 |  |
| pass | time.query | disk@stream/q10 | 114.01 | 27.08 | 0.25 | 3.00 |  |
| pass | time.query | disk@stream/q11 | 0.54 | 0.20 | 0.86 | 3.00 |  |
| pass | time.query | disk@stream/q12 | 0.15 | 0.08 | 0.97 | 3.00 |  |
| pass | time.query | disk@stream/q13 | 172.05 | 33.79 | 0.21 | 3.00 |  |
| pass | time.query | disk@stream/q14 | 60.17 | 23.59 | 0.41 | 3.00 |  |
| pass | time.geomean | disk@stream | – | – | 0.16 | 1.50 | geomean of 10 warm-median ratios |
| pass | time.build | disk@stream | 263743.67 | 3427.05 | 0.01 | 2.50 |  |
| pass | time.open | disk@stream | 1447.34 | 0.76 | 0.01 | 2.50 |  |
| pass | time.query | rocksdb/q1 | 9.47 | 0.03 | 0.18 | 3.00 |  |
| pass | time.query | rocksdb/q2 | 87.10 | 4.57 | 0.07 | 3.00 |  |
| pass | time.query | rocksdb/q3 | 29.34 | 0.04 | 0.07 | 3.00 |  |
| pass | time.query | rocksdb/q4 | 123.27 | 0.14 | 0.02 | 3.00 |  |
| pass | time.query | rocksdb/q5 | 69.86 | 36.12 | 0.53 | 3.00 |  |
| pass | time.query | rocksdb/q6 | 68.19 | 28.59 | 0.44 | 3.00 |  |
| pass | time.query | rocksdb/q7 | 131.77 | 42.44 | 0.33 | 3.00 |  |
| pass | time.query | rocksdb/q8 | 162.09 | 78.00 | 0.49 | 3.00 |  |
| pass | time.query | rocksdb/q9 | 241.39 | 86.32 | 0.36 | 3.00 |  |
| pass | time.query | rocksdb/q10 | 69.61 | 37.63 | 0.55 | 3.00 |  |
| pass | time.query | rocksdb/q11 | 2.71 | 0.77 | 0.59 | 3.00 |  |
| pass | time.query | rocksdb/q12 | 1.52 | 0.14 | 0.61 | 3.00 |  |
| pass | time.query | rocksdb/q13 | 76.77 | 31.80 | 0.43 | 3.00 |  |
| pass | time.query | rocksdb/q14 | 49.97 | 22.65 | 0.47 | 3.00 |  |
| pass | time.geomean | rocksdb | – | – | 0.10 | 1.50 | geomean of 14 warm-median ratios |
| pass | time.build | rocksdb | 53931.02 | 3587.75 | 0.07 | 2.00 |  |
| pass | time.open | rocksdb | 9988.71 | 120.02 | 0.01 | 2.50 |  |
| pass | rss.query_hwm | snapshot | 152.54 | 128.36 | 0.87 | 1.50 |  |
| pass | rss.build_peak | snapshot | 154.20 | 112.77 | 0.78 | 1.50 |  |
| pass | rss.after_open | snapshot | 125.84 | 102.23 | 0.85 | 1.50 |  |
| pass | rss.query_hwm | disk@materialize | 167.48 | 115.48 | 0.74 | 1.50 |  |
| pass | rss.build_peak | disk@materialize | 164.71 | 108.43 | 0.71 | 1.50 |  |
| pass | rss.after_open | disk@materialize | 129.73 | 89.09 | 0.75 | 1.50 |  |
| pass | rss.query_hwm | disk@stream | 167.48 | 65.95 | 0.49 | 1.50 |  |
| pass | rss.build_peak | disk@stream | 164.71 | 96.93 | 0.66 | 1.50 |  |
| pass | rss.after_open | disk@stream | 129.73 | 6.05 | 0.24 | 1.50 |  |
| pass | rss.query_hwm | rocksdb | 184.86 | 101.99 | 0.62 | 1.50 |  |
| pass | rss.build_peak | rocksdb | 285.53 | 92.64 | 0.39 | 1.50 |  |
| pass | rss.after_open | rocksdb | 145.78 | 66.41 | 0.55 | 1.50 |  |
| pass | bytes.dir | snapshot | 85480180.00 | 85480180.00 | 1.00 | 1.25 |  |
| pass | bytes.dir | disk@materialize | 175005702.00 | 130150400.00 | 0.74 | 1.25 |  |
| pass | bytes.dir | disk@stream | 175005702.00 | 130150400.00 | 0.74 | 1.25 |  |
| pass | bytes.dir | rocksdb | 41162410.00 | 31059804.00 | 0.75 | 1.25 |  |
| pass | info.conc_qps | snapshot/sparql/q1/c1 | 2000.98 | 12532.20 | 6.26 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q1/c8 | 2099.42 | 69790.78 | 33.24 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c1 | 81.03 | 5833.94 | 72.00 | – | report-only |
| pass | info.conc_qps | snapshot/sparql/q4/c8 | 76.33 | 30561.39 | 400.37 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c1 | 1616.66 | 5410.49 | 3.35 | – | report-only |
| pass | info.conc_qps | snapshot/api/q1/c8 | 2293.12 | 13856.12 | 6.04 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c1 | 85.62 | 2559.64 | 29.89 | – | report-only |
| pass | info.conc_qps | snapshot/api/q4/c8 | 76.53 | 9957.75 | 130.12 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c1 | 1542.54 | 11552.87 | 7.49 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q1/c8 | 1449.55 | 61370.21 | 42.34 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c1 | 84.87 | 5661.00 | 66.70 | – | report-only |
| pass | info.conc_qps | disk@materialize/sparql/q4/c8 | 79.29 | 31466.18 | 396.87 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c1 | 1211.81 | 5443.28 | 4.49 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q1/c8 | 1460.59 | 15633.83 | 10.70 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c1 | 85.88 | 2508.12 | 29.21 | – | report-only |
| pass | info.conc_qps | disk@materialize/api/q4/c8 | 76.44 | 11465.57 | 150.00 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c1 | 1542.54 | 10774.60 | 6.98 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q1/c8 | 1449.55 | 45735.64 | 31.55 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c1 | 84.87 | 4660.19 | 54.91 | – | report-only |
| pass | info.conc_qps | disk@stream/sparql/q4/c8 | 79.29 | 20254.93 | 255.46 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c1 | 1211.81 | 4604.40 | 3.80 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q1/c8 | 1460.59 | 15193.69 | 10.40 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c1 | 85.88 | 2017.69 | 23.50 | – | report-only |
| pass | info.conc_qps | disk@stream/api/q4/c8 | 76.44 | 8832.27 | 115.55 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c1 | 80.25 | 8800.45 | 109.66 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q1/c8 | 75.32 | 49224.81 | 653.50 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c1 | 7.17 | 2502.64 | 349.01 | – | report-only |
| pass | info.conc_qps | rocksdb/sparql/q4/c8 | 6.30 | 10208.74 | 1621.28 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c1 | 93.56 | 4117.52 | 44.01 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q1/c8 | 77.89 | 11462.24 | 147.15 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c1 | 7.45 | 1383.07 | 185.71 | – | report-only |
| pass | info.conc_qps | rocksdb/api/q4/c8 | 6.68 | 6296.10 | 942.56 | – | report-only |

