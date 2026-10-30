# LLM model benchmark: tables

Generated 2026-10-30T08:11:42+00:00 from `/Users/rjjain/Documents/Restia/Mockingbird/.claude/worktrees/llm-benchmark-f85581/docs/benchmarks/2026-10-30`. Seed `mb-bench-2026-10-29`, judge `claude-opus-5-5`, baseline used `deepseek/deepseek-v4.1-flash` (requested `deepseek/deepseek-v4.1-flash`).

Conventions: `—` means null (missing input or zero denominator). Every table has an n column or states its n. N = engine samples per model; n in the latency table = successful calls; judge_n = judge items incl. auto-scored ones (rubric 1.5).

## 1. Leaderboard (sorted by Value; quality rank alongside)

n = engine samples N per model (column `N`); judge_n = judge items per model (auto included).

| Value rank | Model | Value | Gate | Q | Q rank | S | K | Jn | C $/1k | L p50 ms | Schema valid | Err+TO | judge_n | N | Within 10% of best Q | Recommendable | Value if ungated (diagnostic) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | openai/gpt-6-luna | 160.5 |  | 0.925 | 2 | 0.976 | 1.000 | 0.833 | 0.1978 | 2,660 | 100.0% | 0.0% | 6 | 162 | yes | yes | 160.5 |
| 2 | deepseek/deepseek-v4.1-flash-1017 | 144.0 |  | 0.909 | 4 | 0.925 | 1.000 | 0.839 | 0.3242 | 1,882 | 99.8% | 0.0% | 7 | 162 | yes | yes | 144.0 |
| 3 | nvidia/nemotron-3.5-lightning | 134.6 |  | 0.797 | 11 | 0.798 | 0.987 | 0.679 | 0.1599 | 2,580 | 93.1% | 0.0% | 7 | 162 | no | no | 134.6 |
| 4 | inception/mercury-3 | 128.6 |  | 0.788 | 12 | 0.843 | 1.000 | 0.607 | 0.4352 | 990 | 97.5% | 0.0% | 7 | 162 | no | no | 128.6 |
| 5 | anthropic/claude-haiku-5.5 | 120.4 |  | 0.934 | 1 | 0.933 | 1.000 | 0.893 | 0.4332 | 2,241 | 99.8% | 0.0% | 7 | 162 | yes | yes | 120.4 |
| 6 | nvidia/nemotron-3.5-super-vl | 102.1 |  | 0.887 | 6 | 0.902 | 1.000 | 0.804 | 0.4394 | 2,499 | 99.2% | 0.0% | 7 | 162 | yes | yes | 102.1 |
| 7 | deepseek/deepseek-v4.1-flash | 100.0 |  | 0.911 | 3 | 0.930 | 1.000 | 0.839 | 0.3653 | 3,491 | 99.6% | 0.0% | 7 | 162 | yes | yes | 100.0 |
| 8 | minimax/minimax-m3.1-flash | 88.1 |  | 0.908 | 5 | 0.860 | 1.000 | 0.893 | 0.6067 | 2,671 | 95.9% | 1.1% | 7 | 162 | yes | yes | 88.1 |
| 9 | z-ai/glm-5.3-flash | 79.2 |  | 0.884 | 7 | 0.906 | 1.000 | 0.792 | 0.4086 | 4,411 | 99.8% | 0.0% | 6 | 162 | yes | yes | 79.2 |
| 10 | qwen/qwen3.8-flash | 64.8 |  | 0.864 | 8 | 0.898 | 1.000 | 0.750 | 0.3114 | 7,888 | 100.0% | 1.6% | 7 | 162 | yes | yes | 64.8 |
| 11 | thinkingmachines/inkling-small@nojson | 63.6 |  | 0.842 | 10 | 0.855 | 1.000 | 0.732 | 0.8869 | 2,596 | 97.5% | 0.0% | 7 | 162 | yes | yes | 63.6 |
| 12 | thinkingmachines/inkling@nojson | 32.6 |  | 0.855 | 9 | 0.871 | 1.000 | 0.750 | 2.2113 | 4,202 | 99.2% | 1.5% | 7 | 162 | yes | yes | 32.6 |

Err+TO = ledger error rate + max(ledger timeout rate, engine-side timeout rate from samples). `Value if ungated` applies the default formula ignoring the gates; it is not the rubric's Value.

## 2. Quality components

n = engine samples N (macro rates over api types); judge items = blinded judge items (auto items included in J).

| Model | N | First try (FT) | Schema valid (SV) | Fallback | S | K (conformance) | J | Jn | Realism | Coherence | judge_n | auto | missing | excl. valid {} | fallback: max_tokens | fallback: engine timeout | Q |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 162 | 0.664 | 0.931 | 11.7% | 0.798 | 0.987 | 3.71 | 0.679 | 3.14 | 4.29 | 7 | 1 | 0 | 0 | 0 | 0 | 0.797 |
| inception/mercury-3 | 162 | 0.710 | 0.975 | 3.7% | 0.843 | 1.000 | 3.43 | 0.607 | 3.29 | 3.57 | 7 | 0 | 0 | 0 | 2 | 0 | 0.788 |
| deepseek/deepseek-v4.1-flash-1017 | 162 | 0.852 | 0.998 | 0.6% | 0.925 | 1.000 | 4.36 | 0.839 | 3.71 | 5.00 | 7 | 0 | 0 | 0 | 0 | 0 | 0.909 |
| openai/gpt-6-luna | 162 | 0.953 | 1.000 | 0.0% | 0.976 | 1.000 | 4.33 | 0.833 | 4.00 | 4.67 | 6 | 0 | 0 | 1 | 0 | 0 | 0.925 |
| nvidia/nemotron-3.5-super-vl | 162 | 0.811 | 0.992 | 2.5% | 0.902 | 1.000 | 4.21 | 0.804 | 3.57 | 4.86 | 7 | 0 | 0 | 0 | 0 | 0 | 0.887 |
| z-ai/glm-5.3-flash | 162 | 0.815 | 0.998 | 0.6% | 0.906 | 1.000 | 4.17 | 0.792 | 3.83 | 4.50 | 6 | 0 | 0 | 1 | 0 | 0 | 0.884 |
| deepseek/deepseek-v4.1-flash | 162 | 0.864 | 0.996 | 1.2% | 0.930 | 1.000 | 4.36 | 0.839 | 4.00 | 4.71 | 7 | 0 | 0 | 0 | 0 | 0 | 0.911 |
| qwen/qwen3.8-flash | 162 | 0.796 | 1.000 | 0.0% | 0.898 | 1.000 | 4.00 | 0.750 | 3.29 | 4.71 | 7 | 0 | 0 | 0 | 0 | 0 | 0.864 |
| anthropic/claude-haiku-5.5 | 162 | 0.869 | 0.998 | 0.6% | 0.933 | 1.000 | 4.57 | 0.893 | 4.29 | 4.86 | 7 | 0 | 0 | 0 | 0 | 0 | 0.934 |
| minimax/minimax-m3.1-flash | 162 | 0.762 | 0.959 | 4.9% | 0.860 | 1.000 | 4.57 | 0.893 | 4.43 | 4.71 | 7 | 0 | 0 | 0 | 0 | 1 | 0.908 |
| thinkingmachines/inkling-small@nojson | 162 | 0.735 | 0.975 | 8.0% | 0.855 | 1.000 | 3.93 | 0.732 | 3.43 | 4.43 | 7 | 0 | 0 | 0 | 0 | 0 | 0.842 |
| thinkingmachines/inkling@nojson | 162 | 0.750 | 0.992 | 2.5% | 0.871 | 1.000 | 4.00 | 0.750 | 3.57 | 4.43 | 7 | 0 | 0 | 0 | 0 | 0 | 0.855 |

excl. valid {} = judge items whose body was a schema-valid `{}` written by the model; they were not shown to the judge and are left out of J instead of being auto-scored 1. fallback: max_tokens / engine timeout = fallback samples caused by output truncation at the engine's max_tokens or by the engine's LLM timeout (sample counts).

## 3a. Per model x api type

n = engine samples in that cell; conformance is over its non-fallback samples (conf n in the last column); p50 is the mb-evals latency of those samples.

| Model | api type | n | First try | Schema valid | Fallback | Conformance | conf n | p50 ms |
|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | asyncapi | 15 | 0.467 | 1.000 | 0.0% | 0.933 | 15 | 1,882 |
| nvidia/nemotron-3.5-lightning | graphql | 24 | 0.708 | 0.792 | 20.8% | 1.000 | 19 | 2,846 |
| nvidia/nemotron-3.5-lightning | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 542 |
| nvidia/nemotron-3.5-lightning | rest | 102 | 0.147 | 0.863 | 13.7% | 1.000 | 88 | 3,675 |
| nvidia/nemotron-3.5-lightning | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 693 |
| inception/mercury-3 | asyncapi | 15 | 0.400 | 1.000 | 0.0% | 1.000 | 15 | 653 |
| inception/mercury-3 | graphql | 24 | 0.875 | 0.917 | 8.3% | 1.000 | 22 | 990 |
| inception/mercury-3 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 515 |
| inception/mercury-3 | rest | 102 | 0.275 | 0.961 | 3.9% | 1.000 | 98 | 1,228 |
| inception/mercury-3 | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 440 |
| deepseek/deepseek-v4.1-flash-1017 | asyncapi | 15 | 0.800 | 1.000 | 0.0% | 1.000 | 15 | 1,530 |
| deepseek/deepseek-v4.1-flash-1017 | graphql | 24 | 0.958 | 1.000 | 0.0% | 1.000 | 24 | 2,418 |
| deepseek/deepseek-v4.1-flash-1017 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 349 |
| deepseek/deepseek-v4.1-flash-1017 | rest | 102 | 0.500 | 0.990 | 1.0% | 1.000 | 101 | 2,578 |
| deepseek/deepseek-v4.1-flash-1017 | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 542 |
| openai/gpt-6-luna | asyncapi | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 2,443 |
| openai/gpt-6-luna | graphql | 24 | 1.000 | 1.000 | 0.0% | 1.000 | 24 | 3,898 |
| openai/gpt-6-luna | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 1,123 |
| openai/gpt-6-luna | rest | 102 | 0.765 | 1.000 | 0.0% | 1.000 | 102 | 3,276 |
| openai/gpt-6-luna | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,339 |
| nvidia/nemotron-3.5-super-vl | asyncapi | 15 | 0.733 | 1.000 | 0.0% | 1.000 | 15 | 1,243 |
| nvidia/nemotron-3.5-super-vl | graphql | 24 | 1.000 | 1.000 | 0.0% | 1.000 | 24 | 2,499 |
| nvidia/nemotron-3.5-super-vl | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 531 |
| nvidia/nemotron-3.5-super-vl | rest | 102 | 0.324 | 0.961 | 3.9% | 1.000 | 98 | 3,374 |
| nvidia/nemotron-3.5-super-vl | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 952 |
| z-ai/glm-5.3-flash | asyncapi | 15 | 0.667 | 1.000 | 0.0% | 1.000 | 15 | 1,900 |
| z-ai/glm-5.3-flash | graphql | 24 | 0.917 | 1.000 | 0.0% | 1.000 | 24 | 4,004 |
| z-ai/glm-5.3-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 1,917 |
| z-ai/glm-5.3-flash | rest | 102 | 0.490 | 0.990 | 1.0% | 1.000 | 101 | 6,156 |
| z-ai/glm-5.3-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,193 |
| deepseek/deepseek-v4.1-flash | asyncapi | 15 | 0.800 | 1.000 | 0.0% | 1.000 | 15 | 5,065 |
| deepseek/deepseek-v4.1-flash | graphql | 24 | 1.000 | 1.000 | 0.0% | 1.000 | 24 | 2,348 |
| deepseek/deepseek-v4.1-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 709 |
| deepseek/deepseek-v4.1-flash | rest | 102 | 0.520 | 0.980 | 2.0% | 1.000 | 100 | 3,457 |
| deepseek/deepseek-v4.1-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 3,926 |
| qwen/qwen3.8-flash | asyncapi | 15 | 0.533 | 1.000 | 0.0% | 1.000 | 15 | 4,605 |
| qwen/qwen3.8-flash | graphql | 24 | 0.958 | 1.000 | 0.0% | 1.000 | 24 | 9,420 |
| qwen/qwen3.8-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 1,744 |
| qwen/qwen3.8-flash | rest | 102 | 0.490 | 1.000 | 0.0% | 1.000 | 102 | 9,431 |
| qwen/qwen3.8-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 2,038 |
| anthropic/claude-haiku-5.5 | asyncapi | 15 | 0.867 | 1.000 | 0.0% | 1.000 | 15 | 1,773 |
| anthropic/claude-haiku-5.5 | graphql | 24 | 0.917 | 1.000 | 0.0% | 1.000 | 24 | 2,851 |
| anthropic/claude-haiku-5.5 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 775 |
| anthropic/claude-haiku-5.5 | rest | 102 | 0.627 | 0.990 | 1.0% | 1.000 | 101 | 2,856 |
| anthropic/claude-haiku-5.5 | soap | 15 | 0.933 | 1.000 | 0.0% | 1.000 | 15 | 812 |
| minimax/minimax-m3.1-flash | asyncapi | 15 | 0.667 | 1.000 | 0.0% | 1.000 | 15 | 3,049 |
| minimax/minimax-m3.1-flash | graphql | 24 | 0.750 | 0.833 | 16.7% | 1.000 | 20 | 3,661 |
| minimax/minimax-m3.1-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 431 |
| minimax/minimax-m3.1-flash | rest | 102 | 0.461 | 0.961 | 3.9% | 1.000 | 98 | 3,992 |
| minimax/minimax-m3.1-flash | soap | 15 | 0.933 | 1.000 | 0.0% | 1.000 | 15 | 1,091 |
| thinkingmachines/inkling-small@nojson | asyncapi | 15 | 0.467 | 1.000 | 0.0% | 1.000 | 15 | 1,185 |
| thinkingmachines/inkling-small@nojson | graphql | 24 | 0.917 | 1.000 | 0.0% | 1.000 | 24 | 2,381 |
| thinkingmachines/inkling-small@nojson | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 451 |
| thinkingmachines/inkling-small@nojson | rest | 102 | 0.294 | 0.873 | 12.7% | 1.000 | 89 | 5,984 |
| thinkingmachines/inkling-small@nojson | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 495 |
| thinkingmachines/inkling@nojson | asyncapi | 15 | 0.467 | 1.000 | 0.0% | 1.000 | 15 | 2,389 |
| thinkingmachines/inkling@nojson | graphql | 24 | 1.000 | 1.000 | 0.0% | 1.000 | 24 | 5,145 |
| thinkingmachines/inkling@nojson | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 514 |
| thinkingmachines/inkling@nojson | rest | 102 | 0.284 | 0.961 | 3.9% | 1.000 | 98 | 5,416 |
| thinkingmachines/inkling@nojson | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 2,092 |

## 3b. Per model x domain

n = engine samples in that cell; conformance is over its non-fallback samples (conf n in the last column); p50 is the mb-evals latency of those samples.

| Model | domain | n | First try | Schema valid | Fallback | Conformance | conf n | p50 ms |
|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | devtools | 27 | 0.444 | 0.852 | 14.8% | 1.000 | 23 | 3,205 |
| nvidia/nemotron-3.5-lightning | ecommerce | 48 | 0.542 | 0.958 | 4.2% | 0.978 | 46 | 1,933 |
| nvidia/nemotron-3.5-lightning | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 671 |
| nvidia/nemotron-3.5-lightning | payments | 21 | 0.000 | 0.857 | 14.3% | 1.000 | 18 | 3,135 |
| nvidia/nemotron-3.5-lightning | social-media | 30 | 0.033 | 0.900 | 10.0% | 1.000 | 27 | 4,189 |
| nvidia/nemotron-3.5-lightning | travel | 18 | 0.167 | 0.611 | 38.9% | 1.000 | 11 | 5,600 |
| inception/mercury-3 | devtools | 27 | 0.519 | 0.963 | 3.7% | 1.000 | 26 | 1,052 |
| inception/mercury-3 | ecommerce | 48 | 0.604 | 0.979 | 2.1% | 1.000 | 47 | 780 |
| inception/mercury-3 | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 440 |
| inception/mercury-3 | payments | 21 | 0.048 | 0.952 | 4.8% | 1.000 | 20 | 861 |
| inception/mercury-3 | social-media | 30 | 0.000 | 0.900 | 10.0% | 1.000 | 27 | 1,326 |
| inception/mercury-3 | travel | 18 | 0.778 | 1.000 | 0.0% | 1.000 | 18 | 1,305 |
| deepseek/deepseek-v4.1-flash-1017 | devtools | 27 | 0.556 | 0.963 | 3.7% | 1.000 | 26 | 2,987 |
| deepseek/deepseek-v4.1-flash-1017 | ecommerce | 48 | 0.792 | 1.000 | 0.0% | 1.000 | 48 | 1,607 |
| deepseek/deepseek-v4.1-flash-1017 | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 491 |
| deepseek/deepseek-v4.1-flash-1017 | payments | 21 | 0.286 | 1.000 | 0.0% | 1.000 | 21 | 1,325 |
| deepseek/deepseek-v4.1-flash-1017 | social-media | 30 | 0.400 | 1.000 | 0.0% | 1.000 | 30 | 3,535 |
| deepseek/deepseek-v4.1-flash-1017 | travel | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 2,912 |
| openai/gpt-6-luna | devtools | 27 | 0.889 | 1.000 | 0.0% | 1.000 | 27 | 4,200 |
| openai/gpt-6-luna | ecommerce | 48 | 0.979 | 1.000 | 0.0% | 1.000 | 48 | 2,402 |
| openai/gpt-6-luna | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,339 |
| openai/gpt-6-luna | payments | 21 | 0.714 | 1.000 | 0.0% | 1.000 | 21 | 3,161 |
| openai/gpt-6-luna | social-media | 30 | 0.533 | 1.000 | 0.0% | 1.000 | 30 | 4,434 |
| openai/gpt-6-luna | travel | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,904 |
| nvidia/nemotron-3.5-super-vl | devtools | 27 | 0.556 | 1.000 | 0.0% | 1.000 | 27 | 2,937 |
| nvidia/nemotron-3.5-super-vl | ecommerce | 48 | 0.729 | 1.000 | 0.0% | 1.000 | 48 | 1,854 |
| nvidia/nemotron-3.5-super-vl | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 952 |
| nvidia/nemotron-3.5-super-vl | payments | 21 | 0.143 | 0.952 | 4.8% | 1.000 | 20 | 3,250 |
| nvidia/nemotron-3.5-super-vl | social-media | 30 | 0.100 | 0.900 | 10.0% | 1.000 | 27 | 9,039 |
| nvidia/nemotron-3.5-super-vl | travel | 18 | 0.833 | 1.000 | 0.0% | 1.000 | 18 | 3,217 |
| z-ai/glm-5.3-flash | devtools | 27 | 0.667 | 0.963 | 3.7% | 1.000 | 26 | 5,704 |
| z-ai/glm-5.3-flash | ecommerce | 48 | 0.729 | 1.000 | 0.0% | 1.000 | 48 | 2,814 |
| z-ai/glm-5.3-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,193 |
| z-ai/glm-5.3-flash | payments | 21 | 0.476 | 1.000 | 0.0% | 1.000 | 21 | 4,943 |
| z-ai/glm-5.3-flash | social-media | 30 | 0.433 | 1.000 | 0.0% | 1.000 | 30 | 7,710 |
| z-ai/glm-5.3-flash | travel | 18 | 0.500 | 1.000 | 0.0% | 1.000 | 18 | 7,455 |
| deepseek/deepseek-v4.1-flash | devtools | 27 | 0.556 | 0.963 | 3.7% | 1.000 | 26 | 3,837 |
| deepseek/deepseek-v4.1-flash | ecommerce | 48 | 0.854 | 1.000 | 0.0% | 1.000 | 48 | 2,294 |
| deepseek/deepseek-v4.1-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 3,926 |
| deepseek/deepseek-v4.1-flash | payments | 21 | 0.238 | 1.000 | 0.0% | 1.000 | 21 | 2,012 |
| deepseek/deepseek-v4.1-flash | social-media | 30 | 0.433 | 0.967 | 3.3% | 1.000 | 29 | 4,868 |
| deepseek/deepseek-v4.1-flash | travel | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 5,811 |
| qwen/qwen3.8-flash | devtools | 27 | 0.630 | 1.000 | 0.0% | 1.000 | 27 | 9,916 |
| qwen/qwen3.8-flash | ecommerce | 48 | 0.771 | 1.000 | 0.0% | 1.000 | 48 | 6,555 |
| qwen/qwen3.8-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 2,007 |
| qwen/qwen3.8-flash | payments | 21 | 0.190 | 1.000 | 0.0% | 1.000 | 21 | 7,590 |
| qwen/qwen3.8-flash | social-media | 30 | 0.400 | 1.000 | 0.0% | 1.000 | 30 | 11,297 |
| qwen/qwen3.8-flash | travel | 18 | 0.778 | 1.000 | 0.0% | 1.000 | 18 | 10,210 |
| anthropic/claude-haiku-5.5 | devtools | 27 | 0.519 | 0.963 | 3.7% | 1.000 | 26 | 3,531 |
| anthropic/claude-haiku-5.5 | ecommerce | 48 | 0.958 | 1.000 | 0.0% | 1.000 | 48 | 1,886 |
| anthropic/claude-haiku-5.5 | generic | 18 | 0.944 | 1.000 | 0.0% | 1.000 | 18 | 770 |
| anthropic/claude-haiku-5.5 | payments | 21 | 0.524 | 1.000 | 0.0% | 1.000 | 21 | 1,542 |
| anthropic/claude-haiku-5.5 | social-media | 30 | 0.433 | 1.000 | 0.0% | 1.000 | 30 | 3,992 |
| anthropic/claude-haiku-5.5 | travel | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 3,045 |
| minimax/minimax-m3.1-flash | devtools | 27 | 0.556 | 0.815 | 18.5% | 1.000 | 22 | 6,591 |
| minimax/minimax-m3.1-flash | ecommerce | 48 | 0.792 | 0.979 | 2.1% | 1.000 | 47 | 3,062 |
| minimax/minimax-m3.1-flash | generic | 18 | 0.944 | 1.000 | 0.0% | 1.000 | 18 | 629 |
| minimax/minimax-m3.1-flash | payments | 21 | 0.238 | 1.000 | 0.0% | 1.000 | 21 | 2,221 |
| minimax/minimax-m3.1-flash | social-media | 30 | 0.300 | 0.967 | 3.3% | 1.000 | 29 | 6,398 |
| minimax/minimax-m3.1-flash | travel | 18 | 0.611 | 0.944 | 5.6% | 1.000 | 17 | 2,914 |
| thinkingmachines/inkling-small@nojson | devtools | 27 | 0.593 | 0.926 | 7.4% | 1.000 | 25 | 2,412 |
| thinkingmachines/inkling-small@nojson | ecommerce | 48 | 0.688 | 0.958 | 4.2% | 1.000 | 46 | 15,750 |
| thinkingmachines/inkling-small@nojson | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 476 |
| thinkingmachines/inkling-small@nojson | payments | 21 | 0.000 | 1.000 | 0.0% | 1.000 | 21 | 18,471 |
| thinkingmachines/inkling-small@nojson | social-media | 30 | 0.000 | 0.700 | 30.0% | 1.000 | 21 | 4,105 |
| thinkingmachines/inkling-small@nojson | travel | 18 | 0.722 | 1.000 | 0.0% | 1.000 | 18 | 2,551 |
| thinkingmachines/inkling@nojson | devtools | 27 | 0.556 | 1.000 | 0.0% | 1.000 | 27 | 5,885 |
| thinkingmachines/inkling@nojson | ecommerce | 48 | 0.667 | 0.979 | 2.1% | 1.000 | 47 | 2,763 |
| thinkingmachines/inkling@nojson | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 830 |
| thinkingmachines/inkling@nojson | payments | 21 | 0.095 | 0.952 | 4.8% | 1.000 | 20 | 2,407 |
| thinkingmachines/inkling@nojson | social-media | 30 | 0.033 | 0.933 | 6.7% | 1.000 | 28 | 10,613 |
| thinkingmachines/inkling@nojson | travel | 18 | 0.722 | 1.000 | 0.0% | 1.000 | 18 | 5,647 |

## 4. Conformance by keyword category (non-fallback samples)

n = keyword checks in each cell (`checked`); pass rate = passed / checked.

| Model | format | enum | constraint |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 0.983 (n=233) | 1.000 (n=781) | 1.000 (n=1,775) |
| inception/mercury-3 | 0.993 (n=419) | 1.000 (n=1,397) | 1.000 (n=2,874) |
| deepseek/deepseek-v4.1-flash-1017 | 0.994 (n=463) | 1.000 (n=1,532) | 1.000 (n=3,222) |
| openai/gpt-6-luna | 0.989 (n=283) | 1.000 (n=1,025) | 1.000 (n=2,013) |
| nvidia/nemotron-3.5-super-vl | 0.993 (n=454) | 1.000 (n=1,471) | 1.000 (n=3,157) |
| z-ai/glm-5.3-flash | 0.993 (n=445) | 1.000 (n=1,544) | 1.000 (n=3,097) |
| deepseek/deepseek-v4.1-flash | 0.994 (n=479) | 1.000 (n=1,590) | 1.000 (n=3,236) |
| qwen/qwen3.8-flash | 0.992 (n=365) | 1.000 (n=1,283) | 1.000 (n=2,532) |
| anthropic/claude-haiku-5.5 | 0.992 (n=373) | 1.000 (n=1,258) | 1.000 (n=2,740) |
| minimax/minimax-m3.1-flash | 0.993 (n=442) | 1.000 (n=1,453) | 1.000 (n=2,864) |
| thinkingmachines/inkling-small@nojson | 0.994 (n=354) | 1.000 (n=1,118) | 1.000 (n=2,293) |
| thinkingmachines/inkling@nojson | 0.991 (n=347) | 1.000 (n=1,089) | 1.000 (n=2,458) |

## 5. Latency (proxy-measured LLM call times)

n (engine) = successful engine calls; n (all) = successful calls, all workloads; n tok = calls used for the tok/s median. L = e2e p50 over engine calls.

| Model | n (engine) | TTFT p50 | TTFT p95 | TTFR p50 | e2e p50 (engine) | e2e p95 (engine) | n (all) | e2e p50 (all) | e2e p95 (all) | tok/s (engine) | tok/s (all) | n tok |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 214 | 469 | 986 | — | 2,580 | 6,305 | 238 | 2,584 | 6,921 | 163.4 | 165.3 | 232 |
| inception/mercury-3 | 174 | 526 | 1,174 | — | 990 | 10,553 | 199 | 1,026 | 15,215 | 863.8 | 853.6 | 159 |
| deepseek/deepseek-v4.1-flash-1017 | 166 | 383 | 1,591 | — | 1,882 | 7,237 | 188 | 1,945 | 7,470 | 207.4 | 209.0 | 179 |
| openai/gpt-6-luna | 163 | 945 | 1,774 | — | 2,660 | 7,239 | 183 | 2,833 | 9,587 | 110.5 | 111.8 | 167 |
| nvidia/nemotron-3.5-super-vl | 174 | 598 | 11,299 | — | 2,499 | 14,009 | 199 | 2,541 | 14,766 | 432.7 | 433.4 | 189 |
| z-ai/glm-5.3-flash | 181 | 2,236 | 17,330 | 346 | 4,411 | 21,404 | 202 | 4,445 | 21,404 | 202.0 | 202.5 | 202 |
| deepseek/deepseek-v4.1-flash | 164 | 554 | 5,607 | — | 3,491 | 9,019 | 186 | 3,491 | 9,418 | 186.1 | 188.1 | 176 |
| qwen/qwen3.8-flash | 163 | 1,105 | 6,584 | — | 7,888 | 19,347 | 183 | 8,158 | 20,714 | 77.2 | 77.4 | 177 |
| anthropic/claude-haiku-5.5 | 173 | 513 | 636 | — | 2,241 | 6,143 | 195 | 2,365 | 6,532 | 241.6 | 240.6 | 188 |
| minimax/minimax-m3.1-flash | 191 | 600 | 24,345 | 1,254 | 2,671 | 28,212 | 211 | 2,671 | 28,631 | 205.8 | 211.3 | 180 |
| thinkingmachines/inkling-small@nojson | 186 | 382 | 22,168 | — | 2,596 | 25,661 | 212 | 2,642 | 26,536 | 183.5 | 187.0 | 203 |
| thinkingmachines/inkling@nojson | 177 | 311 | 3,203 | — | 4,202 | 19,802 | 198 | 4,300 | 19,802 | 61.9 | 62.7 | 186 |

## 6. Reliability

n = ledger rows for the model, budget-refused rows excluded. Provider shares are over calls with a served provider.

| Model | n | errors | error rate | timeouts | timeout rate | engine timeouts (of N) | refusal rate | retry rate | reasoning toggle rejected | JSON mode rejected | response_format sent | served providers (share) | n providers |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 238 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:235 | Io Net 46.6%, CoreWeave 30.3%, Darkbloom 20.2%, DeepInfra 2.9% | 238 |
| inception/mercury-3 | 199 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:196 | Inception 100.0% | 199 |
| deepseek/deepseek-v4.1-flash-1017 | 188 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:185 | Wafer 30.3%, InferenceNet 19.7%, Cohere 8.5%, DigitalOcean 8.0%, Morph 7.4%, Ionstream 6.9%, Decart 5.3%, ModelRun 3.7%, Cloudflare 2.1%, StreamLake 2.1%, GMICloud 1.6%, Sail Research 1.6%, Particle AI 1.1%, AtlasCloud 0.5%, Crusoe 0.5%, Fireworks 0.5% | 188 |
| openai/gpt-6-luna | 185 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 1.1% | 0.0% | no | no | json_object:182 | OpenAI 100.0% | 185 |
| nvidia/nemotron-3.5-super-vl | 199 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:196 | Crusoe 32.7%, Darkbloom 23.6%, DeepInfra 12.6%, Together 11.1%, CoreWeave 10.6%, Venice 9.5% | 199 |
| z-ai/glm-5.3-flash | 202 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 2.0% | yes | no | json_object:199 | Wafer 41.6%, OpenInference 29.2%, InferenceNet 8.4%, Particle AI 5.4%, Cohere 3.0%, Reka 2.5%, DigitalOcean 2.0%, CoreWeave 1.5%, Venice 1.5%, Fireworks 1.0%, Morph 1.0%, Relace 1.0%, Crusoe 0.5%, Friendli 0.5%, Makora 0.5%, NextBit 0.5% | 202 |
| deepseek/deepseek-v4.1-flash | 186 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:183 | Wafer 55.9%, InferenceNet 19.9%, CoreWeave 8.6%, DigitalOcean 6.5%, DeepInfra 1.6%, Morph 1.6%, Together 1.6%, Decart 1.1%, Venice 1.1%, Fireworks 0.5%, Makora 0.5%, Parasail 0.5%, Reka 0.5% | 186 |
| qwen/qwen3.8-flash | 186 | 3 | 1.6% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:183 | Alibaba 100.0% | 185 |
| anthropic/claude-haiku-5.5 | 195 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:192 | Claude Platform on AWS 100.0% | 195 |
| minimax/minimax-m3.1-flash | 213 | 1 | 0.5% | 1 | 0.5% | 1 (0.6%) | 0.0% | 1.9% | yes | no | json_object:210 | Parasail 46.2%, GMICloud 25.9%, InferenceNet 10.8%, Minimax 9.9%, Novita 4.2%, SambaNova 2.4%, ModelRun 0.5% | 212 |
| thinkingmachines/inkling-small@nojson | 212 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:23 | DeepInfra 100.0% | 212 |
| thinkingmachines/inkling@nojson | 201 | 3 | 1.5% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:18 | Together 100.0% | 198 |

## 7. Cost

n = engine calls (C, tokens per call); total and actual spend cover all ledger rows of the model.

| Model | engine calls | actual $ (usage) | total $ (all workloads) | engine $ | C $/1k responses | estimate share (engine) | prompt tok/call | completion tok/call | reasoning tok/call | n tok |
|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 214 | 0.0324 | 0.0324 | 0.0259 | 0.1599 | 0.0% | 1,258 | 412 | 0 | 214 |
| inception/mercury-3 | 174 | 0.0976 | 0.0976 | 0.0705 | 0.4352 | 0.0% | 1,343 | 882 | 0 | 174 |
| deepseek/deepseek-v4.1-flash-1017 | 166 | 0.0633 | 0.0633 | 0.0525 | 0.3242 | 0.0% | 990 | 421 | 0 | 166 |
| openai/gpt-6-luna | 163 | 0.0410 | 0.0410 | 0.0320 | 0.1978 | 0.0% | 907 | 254 | 0 | 163 |
| nvidia/nemotron-3.5-super-vl | 174 | 0.0853 | 0.0853 | 0.0712 | 0.4394 | 0.0% | 1,161 | 574 | 0 | 174 |
| z-ai/glm-5.3-flash | 181 | 0.0742 | 0.0742 | 0.0662 | 0.4086 | 0.0% | 987 | 1,095 | 543 | 181 |
| deepseek/deepseek-v4.1-flash | 164 | 0.0718 | 0.0718 | 0.0592 | 0.3653 | 0.0% | 973 | 439 | 0 | 164 |
| qwen/qwen3.8-flash | 163 | 0.0635 | 0.0641 | 0.0505 | 0.3114 | 0.0% | 922 | 507 | 0 | 163 |
| anthropic/claude-haiku-5.5 | 173 | 0.0837 | 0.0837 | 0.0702 | 0.4332 | 0.0% | 1,528 | 506 | 0 | 173 |
| minimax/minimax-m3.1-flash | 191 | 0.1241 | 0.1269 | 0.0983 | 0.6067 | 0.0% | 1,139 | 785 | 430 | 191 |
| thinkingmachines/inkling-small@nojson | 186 | 0.1722 | 0.1722 | 0.1437 | 0.8869 | 0.0% | 1,052 | 389 | 0 | 186 |
| thinkingmachines/inkling@nojson | 180 | 0.4274 | 0.4274 | 0.3582 | 2.2113 | 0.0% | 980 | 292 | 0 | 177 |

## 8. Server suites

n = server checks per model (consistency rows, SSE rows, import inputs); import recall/precision are over the import inputs with an expected list.

| Model | consistency n | case ok | field agreement | SSE n | SSE ok | SSE TTFE p50 | import n | draft valid | recall macro / micro | precision macro / micro | import mean ms | warnings mean | harness-rejected rows |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 18 | 72.2% | 70.8% (fields 65) | 3 | 100.0% | 413 | 4 | 75.0% | 0.750 / 0.692 | 0.750 / 1.000 | 11,631 | 0.0 | 0 |
| inception/mercury-3 | 18 | 77.8% | 75.3% (fields 81) | 3 | 100.0% | 409 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 2,053 | 0.0 | 0 |
| deepseek/deepseek-v4.1-flash-1017 | 18 | 100.0% | 93.8% (fields 81) | 3 | 100.0% | 858 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,224 | 0.0 | 0 |
| openai/gpt-6-luna | 18 | 88.9% | 84.0% (fields 81) | 3 | 100.0% | 874 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 9,156 | 0.0 | 0 |
| nvidia/nemotron-3.5-super-vl | 18 | 83.3% | 74.1% (fields 81) | 3 | 100.0% | 1,250 | 4 | 100.0% | 0.938 / 0.923 | 1.000 / 1.000 | 8,310 | 0.2 | 0 |
| z-ai/glm-5.3-flash | 18 | 83.3% | 77.8% (fields 81) | 3 | 100.0% | 2,669 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,091 | 3.8 | 0 |
| deepseek/deepseek-v4.1-flash | 18 | 100.0% | 92.6% (fields 81) | 3 | 100.0% | 620 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,164 | 0.0 | 0 |
| qwen/qwen3.8-flash | 18 | 94.4% | 87.7% (fields 81) | 3 | 100.0% | 1,703 | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 26,223 | 0.0 | 0 |
| anthropic/claude-haiku-5.5 | 18 | 88.9% | 82.7% (fields 81) | 3 | 100.0% | 708 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,046 | 0.0 | 0 |
| minimax/minimax-m3.1-flash | 18 | 66.7% | 78.5% (fields 65) | 3 | 100.0% | 1,036 | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 26,531 | 0.0 | 0 |
| thinkingmachines/inkling-small@nojson | 18 | 88.9% | 79.0% (fields 81) | 3 | 100.0% | 4,401 | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 20,280 | 0.0 | 0 |
| thinkingmachines/inkling@nojson | 18 | 94.4% | 89.0% (fields 73) | 3 | 100.0% | 504 | 4 | 25.0% | 0.250 / 0.077 | 0.250 / 1.000 | 15,949 | 0.0 | 0 |

harness-rejected rows = server rows Mockingbird refused before any LLM call (the bench's own request was invalid, e.g. `@` from a variant id in the session header) plus the rows skipped because of them; they are left out of every rate above.

Consistency cases by name (n, ok):

| Model | case | n | ok rate |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | square.create | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | square.read | 3 | 33.3% |
| nvidia/nemotron-3.5-lightning | stripe.create | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | stripe.read | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | tmdb.detail | 3 | 0.0% |
| nvidia/nemotron-3.5-lightning | tmdb.list | 3 | 100.0% |
| inception/mercury-3 | square.create | 3 | 66.7% |
| inception/mercury-3 | square.read | 3 | 100.0% |
| inception/mercury-3 | stripe.create | 3 | 100.0% |
| inception/mercury-3 | stripe.read | 3 | 100.0% |
| inception/mercury-3 | tmdb.detail | 3 | 0.0% |
| inception/mercury-3 | tmdb.list | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | square.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | square.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | stripe.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | stripe.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | tmdb.detail | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | tmdb.list | 3 | 100.0% |
| openai/gpt-6-luna | square.create | 3 | 100.0% |
| openai/gpt-6-luna | square.read | 3 | 100.0% |
| openai/gpt-6-luna | stripe.create | 3 | 100.0% |
| openai/gpt-6-luna | stripe.read | 3 | 100.0% |
| openai/gpt-6-luna | tmdb.detail | 3 | 33.3% |
| openai/gpt-6-luna | tmdb.list | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | square.create | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | square.read | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | stripe.create | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | stripe.read | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | tmdb.detail | 3 | 0.0% |
| nvidia/nemotron-3.5-super-vl | tmdb.list | 3 | 100.0% |
| z-ai/glm-5.3-flash | square.create | 3 | 100.0% |
| z-ai/glm-5.3-flash | square.read | 3 | 100.0% |
| z-ai/glm-5.3-flash | stripe.create | 3 | 100.0% |
| z-ai/glm-5.3-flash | stripe.read | 3 | 100.0% |
| z-ai/glm-5.3-flash | tmdb.detail | 3 | 0.0% |
| z-ai/glm-5.3-flash | tmdb.list | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | square.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | square.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | stripe.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | stripe.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | tmdb.detail | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | tmdb.list | 3 | 100.0% |
| qwen/qwen3.8-flash | square.create | 3 | 100.0% |
| qwen/qwen3.8-flash | square.read | 3 | 100.0% |
| qwen/qwen3.8-flash | stripe.create | 3 | 100.0% |
| qwen/qwen3.8-flash | stripe.read | 3 | 100.0% |
| qwen/qwen3.8-flash | tmdb.detail | 3 | 66.7% |
| qwen/qwen3.8-flash | tmdb.list | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | square.create | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | square.read | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | stripe.create | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | stripe.read | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | tmdb.detail | 3 | 33.3% |
| anthropic/claude-haiku-5.5 | tmdb.list | 3 | 100.0% |
| minimax/minimax-m3.1-flash | square.create | 3 | 33.3% |
| minimax/minimax-m3.1-flash | square.read | 3 | 33.3% |
| minimax/minimax-m3.1-flash | stripe.create | 3 | 100.0% |
| minimax/minimax-m3.1-flash | stripe.read | 3 | 100.0% |
| minimax/minimax-m3.1-flash | tmdb.detail | 3 | 33.3% |
| minimax/minimax-m3.1-flash | tmdb.list | 3 | 100.0% |
| thinkingmachines/inkling-small@nojson | square.create | 3 | 100.0% |
| thinkingmachines/inkling-small@nojson | square.read | 3 | 100.0% |
| thinkingmachines/inkling-small@nojson | stripe.create | 3 | 100.0% |
| thinkingmachines/inkling-small@nojson | stripe.read | 3 | 100.0% |
| thinkingmachines/inkling-small@nojson | tmdb.detail | 3 | 33.3% |
| thinkingmachines/inkling-small@nojson | tmdb.list | 3 | 100.0% |
| thinkingmachines/inkling@nojson | square.create | 3 | 100.0% |
| thinkingmachines/inkling@nojson | square.read | 3 | 66.7% |
| thinkingmachines/inkling@nojson | stripe.create | 3 | 100.0% |
| thinkingmachines/inkling@nojson | stripe.read | 3 | 100.0% |
| thinkingmachines/inkling@nojson | tmdb.detail | 3 | 100.0% |
| thinkingmachines/inkling@nojson | tmdb.list | 3 | 100.0% |

## 9. Value sensitivity

Value = 100 (Q/Qb)^a / ((C/Cb)^b (L/Lb)^c). n = models with a Value under each set (gated models show —). Default (a,b,c) = (2, 0.5, 0.5).

| Model | (default) | (1,1,1) | (2,0.25,1) |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 134.6 (rank 3) | 270.5 (rank 1) | 127.4 (rank 5) |
| inception/mercury-3 | 128.6 (rank 4) | 256.0 (rank 2) | 252.4 (rank 1) |
| deepseek/deepseek-v4.1-flash-1017 | 144.0 (rank 2) | 208.6 (rank 4) | 190.3 (rank 2) |
| openai/gpt-6-luna | 160.5 (rank 1) | 246.1 (rank 3) | 157.7 (rank 3) |
| nvidia/nemotron-3.5-super-vl | 102.1 (rank 6) | 113.1 (rank 6) | 126.4 (rank 6) |
| z-ai/glm-5.3-flash | 79.2 (rank 9) | 68.6 (rank 9) | 72.4 (rank 10) |
| deepseek/deepseek-v4.1-flash | 100.0 (rank 7) | 100.0 (rank 7) | 100.0 (rank 8) |
| qwen/qwen3.8-flash | 64.8 (rank 10) | 49.2 (rank 11) | 41.4 (rank 12) |
| anthropic/claude-haiku-5.5 | 120.4 (rank 5) | 134.6 (rank 5) | 156.8 (rank 4) |
| minimax/minimax-m3.1-flash | 88.1 (rank 8) | 78.4 (rank 8) | 114.4 (rank 7) |
| thinkingmachines/inkling-small@nojson | 63.6 (rank 11) | 51.2 (rank 10) | 92.0 (rank 9) |
| thinkingmachines/inkling@nojson | 32.6 (rank 12) | 12.9 (rank 12) | 46.6 (rank 11) |

## 10. Spend

| Scope | n (calls) | USD (ledger cost_usd sum) |
|---|---|---|
| pilot ledger | 124 | 0.0374 |
| full ledger | 2,404 | 1.3399 |
| combined (pilot + full) | 2,528 | 1.3774 |
| superseded ledger (redone runs) | — | — |
| all incl. superseded | 2,528 | 1.3774 |

By workload (n = ledger rows per scope is in the table above):

| Scope | workload | USD |
|---|---|---|
| pilot | pilot | 0.0374 |
| full | consistency | 0.1657 |
| full | engine | 1.0983 |
| full | import | 0.0705 |
| full | sse | 0.0054 |
| combined | consistency | 0.1657 |
| combined | engine | 1.0983 |
| combined | import | 0.0705 |
| combined | pilot | 0.0374 |
| combined | sse | 0.0054 |

Key usage delta (key-after.usage - key-before.usage): 1.3328 USD (before 8.5670, after 9.8998; ledger combined minus key delta 0.0445; ledger incl. superseded minus key delta 0.0445).

## 11. Judge plan and checks

n = judge items 82; planned targets 7; judge model `claude-opus-5-5`; seed `mb-bench-2026-10-29`; order-check MAD 0.062 over 24 pairs (noisy: no).

| stratum | contract | target | api_type | domain | rep |
|---|---|---|---|---|---|
| api_type:rest | rest/travel-booking | createBooking | rest | travel | 0 |
| api_type:graphql | graphql/ecommerce-shopify-admin | Query.shop | graphql | ecommerce | 0 |
| api_type:grpc | grpc/otlp-trace-service | TraceService.Export | grpc | devtools | 0 |
| api_type:soap | soap/accounts-rpc | GetAccount | soap | generic | 0 |
| api_type:asyncapi | asyncapi/orders-amqp-2.6 | orderCreated | asyncapi | ecommerce | 0 |
| domain:travel | rest/travel-booking | getBooking | rest | travel | 0 |
| domain:payments | rest/payments-stripe | PostRefunds | rest | payments | 0 |

judge-key check: 84 key items vs 84 planned, missing 0, extra 0, opaque_id mismatches 0.

## Gates

A model's Value is null when gated: skipped, no_data, schema_valid (SV < 0.90), unreliable (error + timeout > 0.10), unjudged (judge_n < 5), missing_inputs. n = engine samples N.

| Model | N | gate reasons |
|---|---|---|
| nvidia/nemotron-3.5-lightning | 162 | none |
| inception/mercury-3 | 162 | none |
| deepseek/deepseek-v4.1-flash-1017 | 162 | none |
| openai/gpt-6-luna | 162 | none |
| nvidia/nemotron-3.5-super-vl | 162 | none |
| z-ai/glm-5.3-flash | 162 | none |
| deepseek/deepseek-v4.1-flash | 162 | none |
| qwen/qwen3.8-flash | 162 | none |
| anthropic/claude-haiku-5.5 | 162 | none |
| minimax/minimax-m3.1-flash | 162 | none |
| thinkingmachines/inkling-small@nojson | 162 | none |
| thinkingmachines/inkling@nojson | 162 | none |

