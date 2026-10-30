## Single calls

| Model | Arm | Calls | Valid first answer | Fallback | p50 latency ms | Mean prompt tokens | Mean completion tokens | Slots ok | Slots missing | Slots invalid |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `deepseek/deepseek-v4.1-flash-1017` | engine | 72 | 0.472 | 0.014 | 2644 | 1187 | 536 | | | |
| `deepseek/deepseek-v4.1-flash-1017` | numbers | 72 | 1.000 | 0.000 | 3498 | 1345 | 449 | 0.981 | 0.000 | 0.019 |
| `deepseek/deepseek-v4.1-flash-1017` | template | 72 | 1.000 | 0.000 | 2957 | 1317 | 650 | 0.984 | 0.001 | 0.015 |
| `deepseek/deepseek-v4.1-flash-1017` | template-ids | 72 | 1.000 | 0.000 | 3247 | 1328 | 675 | 0.967 | 0.001 | 0.032 |
| `openai/gpt-6-luna` | engine | 72 | 0.889 | 0.000 | 3542 | 1035 | 305 | | | |
| `openai/gpt-6-luna` | numbers | 72 | 1.000 | 0.000 | 3547 | 1320 | 371 | 0.990 | 0.000 | 0.010 |
| `openai/gpt-6-luna` | template | 72 | 1.000 | 0.000 | 5167 | 1246 | 567 | 0.990 | 0.006 | 0.003 |
| `openai/gpt-6-luna` | template-ids | 72 | 1.000 | 0.000 | 5304 | 1250 | 583 | 0.971 | 0.004 | 0.024 |
| `nvidia/nemotron-3.5-lightning` | engine | 72 | 0.292 | 0.125 | 3578 | 1870 | 622 | | | |
| `nvidia/nemotron-3.5-lightning` | numbers | 72 | 0.986 | 0.014 | 5701 | 1474 | 716 | 0.936 | 0.029 | 0.035 |
| `nvidia/nemotron-3.5-lightning` | template | 72 | 0.972 | 0.028 | 3755 | 1365 | 924 | 0.844 | 0.118 | 0.038 |
| `nvidia/nemotron-3.5-lightning` | template-ids | 72 | 1.000 | 0.000 | 4260 | 1352 | 719 | 0.864 | 0.087 | 0.049 |

## Blind judging

| Model | Arm | n | Realism | Coherence |
|---|---|---:|---:|---:|
| `deepseek/deepseek-v4.1-flash-1017` | engine | 12 | 4.42 | 4.50 |
| `deepseek/deepseek-v4.1-flash-1017` | template | 12 | 4.25 | 3.75 |
| `deepseek/deepseek-v4.1-flash-1017` | template-ids | 12 | 4.50 | 3.58 |
| `nvidia/nemotron-3.5-lightning` | engine | 12 | 2.58 | 3.25 |
| `nvidia/nemotron-3.5-lightning` | template | 12 | 2.83 | 2.67 |
| `nvidia/nemotron-3.5-lightning` | template-ids | 12 | 2.92 | 2.58 |
| `openai/gpt-6-luna` | engine | 12 | 3.92 | 4.42 |
| `openai/gpt-6-luna` | template | 12 | 4.08 | 4.00 |
| `openai/gpt-6-luna` | template-ids | 12 | 4.33 | 4.33 |

## Journeys: checks passed

| Journey | Model | Engine | Values and world |
|---|---|---:|---:|
| stripe | `deepseek/deepseek-v4.1-flash-1017` | 14/22 | 18/20 |
| stripe | `openai/gpt-6-luna` | 14/22 | 18/20 |
| stripe | `nvidia/nemotron-3.5-lightning` | 14/22 | 20/21 |
| travel | `deepseek/deepseek-v4.1-flash-1017` | 13/24 | 18/24 |
| travel | `openai/gpt-6-luna` | 12/24 | 20/24 |
| travel | `nvidia/nemotron-3.5-lightning` | 12/24 | 17/24 |
| github | `deepseek/deepseek-v4.1-flash-1017` | 6/12 | 12/12 |
| github | `openai/gpt-6-luna` | 7/12 | 12/12 |
| github | `nvidia/nemotron-3.5-lightning` | 6/12 | 10/12 |

### By check

| Journey | Check | Engine | Values and world |
|---|---|---:|---:|
| stripe | intent keeps the customer | 6/6 | 6/6 |
| stripe | intent keeps the amount | 6/6 | 6/6 |
| stripe | read matches the created intent | 6/6 | 6/6 |
| stripe | confirm keeps identity and amount | 0/6 | 6/6 |
| stripe | confirm moves the status on | 6/6 | 1/6 |
| stripe | a succeeded intent received its amount | 0/6 | 1/1 |
| stripe | read shows the confirmed status | 0/6 | 6/6 |
| stripe | refund names its intent | 6/6 | 6/6 |
| stripe | refund keeps the amount | 6/6 | 6/6 |
| stripe | refund uses the intent's currency | 0/6 | 6/6 |
| stripe | customer reads back as created | 6/6 | 6/6 |
| travel | offers fly the searched route | 6/6 | 6/6 |
| travel | offers use the searched currency | 6/6 | 6/6 |
| travel | offer detail keeps the listed price | 0/6 | 6/6 |
| travel | offer detail keeps the listed segments | 0/6 | 0/6 |
| travel | booking names its offer | 6/6 | 6/6 |
| travel | booking keeps the passenger | 6/6 | 6/6 |
| travel | booking costs what the offer did | 1/6 | 4/6 |
| travel | booking flies the offer's segments | 0/6 | 0/6 |
| travel | read matches the created booking | 6/6 | 6/6 |
| travel | cancellation names its booking | 6/6 | 6/6 |
| travel | refund is in the booking's currency | 0/6 | 6/6 |
| travel | cancel shows on the booking | 0/6 | 3/6 |
| github | repo detail keeps the listed repo | 0/6 | 6/6 |
| github | issue keeps its title | 6/6 | 6/6 |
| github | issue reads back as created | 6/6 | 6/6 |
| github | issue list holds the new issue | 6/6 | 6/6 |
| github | repo keeps its identity | 0/6 | 6/6 |
| github | repo counts the new issue | 1/6 | 4/6 |

engine: 98/174 checks, 90 generated answers of 114 calls, p50 3016 ms.

template: 145/169 checks, 66 generated answers of 114 calls, p50 3230 ms.

Effects: 13 applied, 1 rejected.
