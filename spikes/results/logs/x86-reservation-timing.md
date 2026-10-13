<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

## x86-64 bench: reservation-timing

### Host

```
Linux 7.0.0-30-generic x86_64
cpu: AMD EPYC-Genoa Processor
cpus: 16
avx2 avx512f 
MemTotal:       32078280 kB
load: 2.39 0.97 0.37
```

### VOS

- `https://codeberg.org/pandres95/VOS.git` `agent/recompiled-agent-execution`

```
65f3b74 feat(agent): compact sequenced invocation acknowledgements
3140c3d feat(pvm): opt-in recompiled Refine execution for Agents
```

Both program limits raised to 2 560 KiB in the checkout (F-005's tree, §13's "before").

| Built | Bytes |
|---|---:|
| core guest ELF | 3859512 |
| core package (`core-full.vos`) | 1969946 |
| runtime (`agent_runtime-limits.pvm`) | 1081973 |

`aledgent-core-host package`: package 1969946 B, program 1968379 B.

Toolchains: host and guests `rustc 1.96.0-nightly (bcf3d36c9 2026-03-19)`.

### Lockstep

`F025_LOCKSTEP=1 F025_BACKEND=recompiler aledgent-core-host measure ... --n 100 --warmup 3`: exit 0,
**75 executions identical** on both engines (exit, counter, registers, gas, output bytes), divergence: **no**.

### Timing (µs unless gas; nearest rank)

| Run | Samples | sync p50 | sync p95 | sync p99 | invoke p50 | invoke p99 | invoke gas p50 | actor gas p50 | ack p50 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| core-interpreter-1 | 26 | 2957765 | 3020037 | 3080571 | 2916788 | 3043651 | 1487285387 | 173414852 | 34273 |
| core-recompiler-1 | 26 | 236662 | 239405 | 240202 | 227735 | 230832 | 1487285387 | 173414852 | 4928 |
| core-interpreter-2 | 26 | 2947438 | 3097200 | 3103721 | 2904864 | 3065753 | 1487285387 | 173414852 | 34855 |
| core-recompiler-2 | 26 | 236269 | 238045 | 239389 | 227341 | 230421 | 1487285387 | 173414852 | 4968 |
| core-lockstep | 26 | 3339960 | 3463813 | 3487411 | 227499 | 230322 | 1487285387 | 173414852 | 5159 |

The lockstep row includes the second engine's cold run in `sync`; it is not a timing.

<details><summary>core-interpreter-1.txt</summary>

```
metric,p50,p95,p99,max (n = 26, warm-up 3, budget 26624 B)
ack_gas,33445503,37196520,37252039,37252039
ack_us,34273,40627,41064,41064
actor_gas,173414852,185906066,186238268,186238268
blob_bytes,1981,2181,2277,2277
caller_us,2957403,3019565,3080165,3080165
invoke_decode_us,12,14,14,14
invoke_encode_us,2020,2084,2330,2330
invoke_execution_us,2914581,2974499,3041607,3041607
invoke_gas,1487285387,1504480169,1504679710,1504679710
invoke_input_bytes,1995724,2008100,2008826,2008826
invoke_output_bytes,26682,38802,39632,39632
invoke_preparation_us,2075,2207,3852,3852
invoke_us,2916788,2976646,3043651,3043651
message_bytes,187,187,188,188
native_us,1175,1215,1225,1225
state_bytes,23711,35831,36661,36661
sync_us,2957765,3020037,3080571,3080571
hot: inputs 33, service image 25491 B in 1 chunks, budget 26624 B
```
</details>

<details><summary>core-interpreter-2.txt</summary>

```
metric,p50,p95,p99,max (n = 26, warm-up 3, budget 26624 B)
ack_gas,33445503,37196520,37252039,37252039
ack_us,34855,39622,40588,40588
actor_gas,173414852,185906066,186238268,186238268
blob_bytes,1981,2181,2277,2277
caller_us,2946939,3096725,3103292,3103292
invoke_decode_us,12,14,14,14
invoke_encode_us,2020,2046,2049,2049
invoke_execution_us,2902573,3051114,3063642,3063642
invoke_gas,1487285387,1504480169,1504679710,1504679710
invoke_input_bytes,1995724,2008100,2008826,2008826
invoke_output_bytes,26682,38802,39632,39632
invoke_preparation_us,2149,2256,2291,2291
invoke_us,2904864,3053347,3065753,3065753
message_bytes,187,187,188,188
native_us,1190,1239,1240,1240
state_bytes,23711,35831,36661,36661
sync_us,2947438,3097200,3103721,3103721
hot: inputs 33, service image 25491 B in 1 chunks, budget 26624 B
```
</details>

<details><summary>core-lockstep.txt</summary>

```
metric,p50,p95,p99,max (n = 26, warm-up 3, budget 26624 B)
ack_gas,33445503,37196520,37252039,37252039
ack_us,5159,5611,5722,5722
actor_gas,173414852,185906066,186238268,186238268
blob_bytes,1981,2181,2277,2277
caller_us,3339578,3463334,3486890,3486890
invoke_decode_us,9,10,10,10
invoke_encode_us,2097,2123,2142,2142
invoke_execution_us,224841,227031,227907,227907
invoke_gas,1487285387,1504480169,1504679710,1504679710
invoke_input_bytes,1995724,2008100,2008826,2008826
invoke_output_bytes,26682,38802,39632,39632
invoke_preparation_us,2490,2632,2657,2657
invoke_us,227499,229592,230322,230322
message_bytes,187,187,188,188
native_us,1176,1225,1225,1225
state_bytes,23711,35831,36661,36661
sync_us,3339960,3463813,3487411,3487411
hot: inputs 33, service image 25491 B in 1 chunks, budget 26624 B
```
</details>

<details><summary>core-recompiler-1.txt</summary>

```
metric,p50,p95,p99,max (n = 26, warm-up 3, budget 26624 B)
ack_gas,33445503,37196520,37252039,37252039
ack_us,4928,5432,5462,5462
actor_gas,173414852,185906066,186238268,186238268
blob_bytes,1981,2181,2277,2277
caller_us,236214,238958,239717,239717
invoke_decode_us,12,22,22,22
invoke_encode_us,2128,2171,2212,2212
invoke_execution_us,225378,227733,228247,228247
invoke_gas,1487285387,1504480169,1504679710,1504679710
invoke_input_bytes,1995724,2008100,2008826,2008826
invoke_output_bytes,26682,38802,39632,39632
invoke_preparation_us,2428,2585,2600,2600
invoke_us,227735,230219,230832,230832
message_bytes,187,187,188,188
native_us,1177,1227,1232,1232
state_bytes,23711,35831,36661,36661
sync_us,236662,239405,240202,240202
hot: inputs 33, service image 25491 B in 1 chunks, budget 26624 B
```
</details>

<details><summary>core-recompiler-2.txt</summary>

```
metric,p50,p95,p99,max (n = 26, warm-up 3, budget 26624 B)
ack_gas,33445503,37196520,37252039,37252039
ack_us,4968,5385,5394,5394
actor_gas,173414852,185906066,186238268,186238268
blob_bytes,1981,2181,2277,2277
caller_us,235821,237566,238935,238935
invoke_decode_us,12,14,14,14
invoke_encode_us,2125,2159,2165,2165
invoke_execution_us,224743,226035,227982,227982
invoke_gas,1487285387,1504480169,1504679710,1504679710
invoke_input_bytes,1995724,2008100,2008826,2008826
invoke_output_bytes,26682,38802,39632,39632
invoke_preparation_us,2444,2633,2636,2636
invoke_us,227341,228631,230421,230421
message_bytes,187,187,188,188
native_us,1186,1236,1236,1236
state_bytes,23711,35831,36661,36661
sync_us,236269,238045,239389,239389
hot: inputs 33, service image 25491 B in 1 chunks, budget 26624 B
```
</details>

Result: exit 0.
