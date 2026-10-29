# LLM model benchmark: tables

Generated 2026-10-29T20:19:51+00:00 from `/Users/rjjain/Documents/Restia/Mockingbird/.claude/worktrees/llm-benchmark-f85581/docs/benchmarks/2026-10-29`. Seed `mb-bench-2026-10-29`, judge `claude-opus-5-5`, baseline used `deepseek/deepseek-v4.1-flash` (requested `deepseek/deepseek-v4.1-flash`).

Conventions: `—` means null (missing input or zero denominator). Every table has an n column or states its n. N = engine samples per model; n in the latency table = successful calls; judge_n = judge items incl. auto-scored ones (rubric 1.5).

## 1. Leaderboard (sorted by Value; quality rank alongside)

n = engine samples N per model (column `N`); judge_n = judge items per model (auto included).

| Value rank | Model | Value | Gate | Q | Q rank | S | K | Jn | C $/1k | L p50 ms | Schema valid | Err+TO | judge_n | N | Within 10% of best Q | Recommendable | Value if ungated (diagnostic) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | openai/gpt-6-luna | 125.1 |  | 0.882 | 5 | 0.906 | 0.994 | 0.792 | 0.2378 | 2,469 | 94.9% | 0.0% | 6 | 162 | yes | yes | 125.1 |
| 2 | deepseek/deepseek-v4.1-flash | 100.0 |  | 0.890 | 3 | 0.874 | 0.994 | 0.839 | 0.4322 | 2,204 | 92.9% | 0.0% | 7 | 162 | yes | yes | 100.0 |
| 3 | deepseek/deepseek-v4.1-flash-1017 | 87.4 |  | 0.901 | 1 | 0.865 | 0.994 | 0.875 | 0.4947 | 2,649 | 93.6% | 0.0% | 7 | 162 | yes | yes | 87.4 |
| 4 | anthropic/claude-haiku-5.5 | 84.1 |  | 0.899 | 2 | 0.900 | 0.994 | 0.839 | 0.5598 | 2,502 | 93.0% | 0.0% | 7 | 162 | yes | yes | 84.1 |
| 5 | thinkingmachines/inkling@nojson | 31.9 |  | 0.828 | 9 | 0.782 | 0.990 | 0.768 | 2.9225 | 2,403 | 90.9% | 0.0% | 7 | 162 | yes | yes | 31.9 |
| — | minimax/minimax-m3.1-flash | — | schema_valid | 0.886 | 4 | 0.775 | 0.993 | 0.917 | 0.5426 | 1,681 | 88.9% | 0.6% | 6 | 162 | yes | no | 101.3 |
| — | google/gemini-3.8-flash | — | schema_valid | 0.850 | 6 | 0.824 | 0.990 | 0.786 | 6.1884 | 9,588 | 82.6% | 0.6% | 7 | 162 | yes | no | 11.6 |
| — | z-ai/glm-5.3-flash | — | schema_valid | 0.843 | 7 | 0.784 | 0.987 | 0.804 | 0.2907 | 7,044 | 85.3% | 4.9% | 7 | 162 | yes | no | 61.1 |
| — | qwen/qwen3.8-flash | — | schema_valid | 0.842 | 8 | 0.795 | 0.971 | 0.804 | 0.3784 | 7,270 | 89.8% | 2.4% | 7 | 162 | yes | no | 52.7 |
| — | thinkingmachines/inkling-small@nojson | — | schema_valid | 0.825 | 10 | 0.734 | 0.987 | 0.804 | 1.1748 | 1,568 | 84.6% | 0.0% | 7 | 162 | yes | no | 61.8 |
| — | inception/mercury-3 | — | schema_valid | 0.819 | 11 | 0.713 | 0.965 | 0.821 | 0.3317 | 1,096 | 83.7% | 0.0% | 7 | 162 | yes | no | 137.1 |
| — | nvidia/nemotron-3.5-super-vl | — | schema_valid | 0.810 | 12 | 0.771 | 0.989 | 0.732 | 0.5388 | 2,882 | 87.2% | 0.0% | 7 | 162 | no | no | 64.9 |
| — | thinkingmachines/inkling-small | — | schema_valid | 0.778 | 13 | 0.680 | 0.988 | 0.732 | 1.3198 | 2,458 | 80.3% | 0.0% | 7 | 162 | no | no | 41.4 |
| — | reflection/beam-1.0-501b-a23b | — | schema_valid | 0.739 | 14 | 0.709 | 0.990 | 0.607 | 1.9635 | 4,883 | 72.2% | 0.5% | 7 | 162 | no | no | 21.7 |
| — | nvidia/nemotron-3.5-lightning | — | schema_valid | 0.719 | 15 | 0.646 | 0.971 | 0.625 | 0.2626 | 1,728 | 74.6% | 0.0% | 7 | 162 | no | no | 94.5 |
| — | thinkingmachines/inkling | — | schema_valid | — | — | 0.000 | — | 0.000 | 6.4591 | 3,917 | 0.0% | 0.0% | 7 | 162 | no | no | — |

Err+TO = ledger error rate + max(ledger timeout rate, engine-side timeout rate from samples). `Value if ungated` applies the default formula ignoring the gates; it is not the rubric's Value.

## 2. Quality components

n = engine samples N (macro rates over api types); judge items = blinded judge items (auto items included in J).

| Model | N | First try (FT) | Schema valid (SV) | Fallback | S | K (conformance) | J | Jn | Realism | Coherence | judge_n | auto | missing | excl. valid {} | fallback: max_tokens | fallback: engine timeout | Q |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 162 | 0.547 | 0.746 | 29.6% | 0.646 | 0.971 | 3.50 | 0.625 | 2.86 | 4.14 | 7 | 0 | 0 | 0 | 3 | 0 | 0.719 |
| deepseek/deepseek-v4.1-flash | 162 | 0.819 | 0.929 | 6.2% | 0.874 | 0.994 | 4.36 | 0.839 | 3.86 | 4.86 | 7 | 0 | 0 | 0 | 4 | 0 | 0.890 |
| inception/mercury-3 | 162 | 0.589 | 0.837 | 14.2% | 0.713 | 0.965 | 4.29 | 0.821 | 4.14 | 4.43 | 7 | 0 | 0 | 0 | 15 | 0 | 0.819 |
| anthropic/claude-haiku-5.5 | 162 | 0.869 | 0.930 | 8.0% | 0.900 | 0.994 | 4.36 | 0.839 | 4.00 | 4.71 | 7 | 0 | 0 | 0 | 3 | 0 | 0.899 |
| openai/gpt-6-luna | 162 | 0.863 | 0.949 | 6.2% | 0.906 | 0.994 | 4.17 | 0.792 | 4.17 | 4.17 | 6 | 0 | 0 | 1 | 0 | 0 | 0.882 |
| qwen/qwen3.8-flash | 162 | 0.691 | 0.898 | 8.0% | 0.795 | 0.971 | 4.21 | 0.804 | 3.71 | 4.71 | 7 | 0 | 0 | 0 | 8 | 3 | 0.842 |
| nvidia/nemotron-3.5-super-vl | 162 | 0.671 | 0.872 | 14.2% | 0.771 | 0.989 | 3.93 | 0.732 | 3.57 | 4.29 | 7 | 0 | 0 | 0 | 13 | 0 | 0.810 |
| z-ai/glm-5.3-flash | 162 | 0.716 | 0.853 | 16.7% | 0.784 | 0.987 | 4.21 | 0.804 | 4.00 | 4.43 | 7 | 1 | 0 | 0 | 12 | 8 | 0.843 |
| deepseek/deepseek-v4.1-flash-1017 | 162 | 0.794 | 0.936 | 6.2% | 0.865 | 0.994 | 4.50 | 0.875 | 4.14 | 4.86 | 7 | 0 | 0 | 0 | 3 | 0 | 0.901 |
| minimax/minimax-m3.1-flash | 162 | 0.662 | 0.889 | 13.0% | 0.775 | 0.993 | 4.67 | 0.917 | 4.33 | 5.00 | 6 | 0 | 0 | 1 | 18 | 1 | 0.886 |
| reflection/beam-1.0-501b-a23b | 162 | 0.697 | 0.722 | 48.8% | 0.709 | 0.990 | 3.43 | 0.607 | 3.00 | 3.86 | 7 | 2 | 0 | 0 | 78 | 0 | 0.739 |
| google/gemini-3.8-flash | 162 | 0.822 | 0.826 | 32.7% | 0.824 | 0.990 | 4.14 | 0.786 | 3.86 | 4.43 | 7 | 1 | 0 | 0 | 52 | 1 | 0.850 |
| thinkingmachines/inkling-small@nojson | 162 | 0.622 | 0.846 | 16.7% | 0.734 | 0.987 | 4.21 | 0.804 | 3.86 | 4.57 | 7 | 0 | 0 | 0 | 7 | 0 | 0.825 |
| thinkingmachines/inkling-small | 162 | 0.556 | 0.803 | 16.7% | 0.680 | 0.988 | 3.93 | 0.732 | 3.57 | 4.29 | 7 | 0 | 0 | 0 | 16 | 0 | 0.778 |
| thinkingmachines/inkling@nojson | 162 | 0.655 | 0.909 | 8.6% | 0.782 | 0.990 | 4.07 | 0.768 | 3.57 | 4.57 | 7 | 0 | 0 | 0 | 1 | 0 | 0.828 |
| thinkingmachines/inkling | 162 | 0.000 | 0.000 | 100.0% | 0.000 | — | 1.00 | 0.000 | 1.00 | 1.00 | 7 | 7 | 0 | 0 | 14 | 0 | — |

excl. valid {} = judge items whose body was a schema-valid `{}` written by the model; they were not shown to the judge and are left out of J instead of being auto-scored 1. fallback: max_tokens / engine timeout = fallback samples caused by output truncation at the engine's max_tokens or by the engine's LLM timeout (sample counts).

## 3a. Per model x api type

n = engine samples in that cell; conformance is over its non-fallback samples (conf n in the last column); p50 is the mb-evals latency of those samples.

| Model | api type | n | First try | Schema valid | Fallback | Conformance | conf n | p50 ms |
|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | asyncapi | 15 | 0.400 | 0.733 | 26.7% | 0.909 | 11 | 1,283 |
| nvidia/nemotron-3.5-lightning | graphql | 24 | 0.167 | 0.250 | 75.0% | 1.000 | 6 | 4,742 |
| nvidia/nemotron-3.5-lightning | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 588 |
| nvidia/nemotron-3.5-lightning | rest | 102 | 0.167 | 0.745 | 25.5% | 0.947 | 76 | 2,520 |
| nvidia/nemotron-3.5-lightning | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 441 |
| deepseek/deepseek-v4.1-flash | asyncapi | 15 | 0.800 | 1.000 | 0.0% | 1.000 | 15 | 1,239 |
| deepseek/deepseek-v4.1-flash | graphql | 24 | 0.667 | 0.667 | 33.3% | 1.000 | 16 | 3,451 |
| deepseek/deepseek-v4.1-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 466 |
| deepseek/deepseek-v4.1-flash | rest | 102 | 0.627 | 0.980 | 2.0% | 0.970 | 100 | 2,662 |
| deepseek/deepseek-v4.1-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 608 |
| inception/mercury-3 | asyncapi | 15 | 0.400 | 1.000 | 0.0% | 0.867 | 15 | 614 |
| inception/mercury-3 | graphql | 24 | 0.375 | 0.375 | 62.5% | 1.000 | 9 | 3,247 |
| inception/mercury-3 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 488 |
| inception/mercury-3 | rest | 102 | 0.304 | 0.941 | 5.9% | 0.958 | 96 | 1,243 |
| inception/mercury-3 | soap | 15 | 0.867 | 0.867 | 13.3% | 1.000 | 13 | 463 |
| anthropic/claude-haiku-5.5 | asyncapi | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,723 |
| anthropic/claude-haiku-5.5 | graphql | 24 | 0.708 | 0.708 | 29.2% | 1.000 | 17 | 2,967 |
| anthropic/claude-haiku-5.5 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 768 |
| anthropic/claude-haiku-5.5 | rest | 102 | 0.637 | 0.941 | 5.9% | 0.969 | 96 | 2,886 |
| anthropic/claude-haiku-5.5 | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 825 |
| openai/gpt-6-luna | asyncapi | 15 | 0.867 | 1.000 | 0.0% | 1.000 | 15 | 2,218 |
| openai/gpt-6-luna | graphql | 24 | 0.792 | 0.792 | 20.8% | 1.000 | 19 | 3,508 |
| openai/gpt-6-luna | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 931 |
| openai/gpt-6-luna | rest | 102 | 0.657 | 0.951 | 4.9% | 0.969 | 97 | 3,302 |
| openai/gpt-6-luna | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,255 |
| qwen/qwen3.8-flash | asyncapi | 15 | 0.467 | 1.000 | 0.0% | 0.933 | 15 | 4,353 |
| qwen/qwen3.8-flash | graphql | 24 | 0.500 | 0.500 | 50.0% | 1.000 | 12 | 18,721 |
| qwen/qwen3.8-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 1,669 |
| qwen/qwen3.8-flash | rest | 102 | 0.490 | 0.990 | 1.0% | 0.921 | 101 | 8,259 |
| qwen/qwen3.8-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,625 |
| nvidia/nemotron-3.5-super-vl | asyncapi | 15 | 0.533 | 1.000 | 0.0% | 1.000 | 15 | 1,223 |
| nvidia/nemotron-3.5-super-vl | graphql | 24 | 0.458 | 0.458 | 54.2% | 1.000 | 11 | 7,293 |
| nvidia/nemotron-3.5-super-vl | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 348 |
| nvidia/nemotron-3.5-super-vl | rest | 102 | 0.363 | 0.902 | 9.8% | 0.946 | 92 | 3,803 |
| nvidia/nemotron-3.5-super-vl | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 792 |
| z-ai/glm-5.3-flash | asyncapi | 15 | 0.800 | 0.933 | 6.7% | 1.000 | 14 | 3,204 |
| z-ai/glm-5.3-flash | graphql | 24 | 0.417 | 0.458 | 54.2% | 1.000 | 11 | 11,983 |
| z-ai/glm-5.3-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 2,925 |
| z-ai/glm-5.3-flash | rest | 102 | 0.363 | 0.873 | 12.7% | 0.933 | 89 | 10,419 |
| z-ai/glm-5.3-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 1,858 |
| deepseek/deepseek-v4.1-flash-1017 | asyncapi | 15 | 0.733 | 1.000 | 0.0% | 1.000 | 15 | 1,301 |
| deepseek/deepseek-v4.1-flash-1017 | graphql | 24 | 0.708 | 0.708 | 29.2% | 1.000 | 17 | 3,958 |
| deepseek/deepseek-v4.1-flash-1017 | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 598 |
| deepseek/deepseek-v4.1-flash-1017 | rest | 102 | 0.529 | 0.971 | 2.9% | 0.970 | 99 | 3,310 |
| deepseek/deepseek-v4.1-flash-1017 | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 836 |
| minimax/minimax-m3.1-flash | asyncapi | 15 | 0.600 | 1.000 | 0.0% | 1.000 | 15 | 2,018 |
| minimax/minimax-m3.1-flash | graphql | 24 | 0.375 | 0.542 | 45.8% | 1.000 | 13 | 2,812 |
| minimax/minimax-m3.1-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 585 |
| minimax/minimax-m3.1-flash | rest | 102 | 0.333 | 0.902 | 9.8% | 0.967 | 92 | 2,433 |
| minimax/minimax-m3.1-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 632 |
| reflection/beam-1.0-501b-a23b | asyncapi | 15 | 0.933 | 1.000 | 0.0% | 1.000 | 15 | 4,418 |
| reflection/beam-1.0-501b-a23b | graphql | 24 | 0.417 | 0.417 | 58.3% | 1.000 | 10 | 4,719 |
| reflection/beam-1.0-501b-a23b | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 2,263 |
| reflection/beam-1.0-501b-a23b | rest | 102 | 0.333 | 0.392 | 60.8% | 0.950 | 40 | 5,363 |
| reflection/beam-1.0-501b-a23b | soap | 15 | 0.800 | 0.800 | 20.0% | 1.000 | 12 | 1,037 |
| google/gemini-3.8-flash | asyncapi | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 6,997 |
| google/gemini-3.8-flash | graphql | 24 | 0.542 | 0.542 | 45.8% | 1.000 | 13 | 10,442 |
| google/gemini-3.8-flash | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 3,063 |
| google/gemini-3.8-flash | rest | 102 | 0.569 | 0.588 | 41.2% | 0.950 | 60 | 10,610 |
| google/gemini-3.8-flash | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 3,144 |
| thinkingmachines/inkling-small@nojson | asyncapi | 15 | 0.400 | 0.933 | 6.7% | 1.000 | 14 | 1,081 |
| thinkingmachines/inkling-small@nojson | graphql | 24 | 0.417 | 0.417 | 58.3% | 1.000 | 10 | 3,380 |
| thinkingmachines/inkling-small@nojson | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 539 |
| thinkingmachines/inkling-small@nojson | rest | 102 | 0.294 | 0.882 | 11.8% | 0.933 | 90 | 1,995 |
| thinkingmachines/inkling-small@nojson | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 506 |
| thinkingmachines/inkling-small | asyncapi | 15 | 0.400 | 0.867 | 13.3% | 1.000 | 13 | 2,007 |
| thinkingmachines/inkling-small | graphql | 24 | 0.167 | 0.208 | 79.2% | 1.000 | 5 | 7,860 |
| thinkingmachines/inkling-small | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 628 |
| thinkingmachines/inkling-small | rest | 102 | 0.216 | 0.941 | 5.9% | 0.938 | 96 | 2,947 |
| thinkingmachines/inkling-small | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 540 |
| thinkingmachines/inkling@nojson | asyncapi | 15 | 0.400 | 1.000 | 0.0% | 1.000 | 15 | 1,498 |
| thinkingmachines/inkling@nojson | graphql | 24 | 0.500 | 0.583 | 41.7% | 1.000 | 14 | 4,537 |
| thinkingmachines/inkling@nojson | grpc | 6 | 1.000 | 1.000 | 0.0% | 1.000 | 6 | 400 |
| thinkingmachines/inkling@nojson | rest | 102 | 0.373 | 0.961 | 3.9% | 0.949 | 98 | 3,196 |
| thinkingmachines/inkling@nojson | soap | 15 | 1.000 | 1.000 | 0.0% | 1.000 | 15 | 491 |
| thinkingmachines/inkling | asyncapi | 15 | 0.000 | 0.000 | 100.0% | — | 0 | 5,890 |
| thinkingmachines/inkling | graphql | 24 | 0.000 | 0.000 | 100.0% | — | 0 | 18,905 |
| thinkingmachines/inkling | grpc | 6 | 0.000 | 0.000 | 100.0% | — | 0 | 1,488 |
| thinkingmachines/inkling | rest | 102 | 0.000 | 0.000 | 100.0% | — | 0 | 11,862 |
| thinkingmachines/inkling | soap | 15 | 0.000 | 0.000 | 100.0% | — | 0 | 1,199 |

## 3b. Per model x domain

n = engine samples in that cell; conformance is over its non-fallback samples (conf n in the last column); p50 is the mb-evals latency of those samples.

| Model | domain | n | First try | Schema valid | Fallback | Conformance | conf n | p50 ms |
|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | devtools | 27 | 0.148 | 0.481 | 51.9% | 1.000 | 13 | 4,366 |
| nvidia/nemotron-3.5-lightning | ecommerce | 48 | 0.375 | 0.771 | 22.9% | 0.865 | 37 | 1,481 |
| nvidia/nemotron-3.5-lightning | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 441 |
| nvidia/nemotron-3.5-lightning | payments | 21 | 0.095 | 0.571 | 42.9% | 1.000 | 12 | 1,510 |
| nvidia/nemotron-3.5-lightning | social-media | 30 | 0.067 | 0.833 | 16.7% | 1.000 | 25 | 3,364 |
| nvidia/nemotron-3.5-lightning | travel | 18 | 0.222 | 0.500 | 50.0% | 1.000 | 9 | 3,904 |
| deepseek/deepseek-v4.1-flash | devtools | 27 | 0.407 | 0.778 | 22.2% | 1.000 | 21 | 3,537 |
| deepseek/deepseek-v4.1-flash | ecommerce | 48 | 0.792 | 0.938 | 6.2% | 0.933 | 45 | 1,904 |
| deepseek/deepseek-v4.1-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 598 |
| deepseek/deepseek-v4.1-flash | payments | 21 | 0.476 | 1.000 | 0.0% | 1.000 | 21 | 1,754 |
| deepseek/deepseek-v4.1-flash | social-media | 30 | 0.633 | 0.967 | 3.3% | 1.000 | 29 | 3,390 |
| deepseek/deepseek-v4.1-flash | travel | 18 | 0.944 | 1.000 | 0.0% | 1.000 | 18 | 2,662 |
| inception/mercury-3 | devtools | 27 | 0.259 | 0.667 | 33.3% | 1.000 | 18 | 1,923 |
| inception/mercury-3 | ecommerce | 48 | 0.542 | 0.833 | 16.7% | 0.875 | 40 | 833 |
| inception/mercury-3 | generic | 18 | 0.889 | 0.889 | 11.1% | 1.000 | 16 | 463 |
| inception/mercury-3 | payments | 21 | 0.048 | 0.952 | 4.8% | 1.000 | 20 | 833 |
| inception/mercury-3 | social-media | 30 | 0.033 | 0.900 | 10.0% | 1.000 | 27 | 1,467 |
| inception/mercury-3 | travel | 18 | 0.778 | 1.000 | 0.0% | 0.944 | 18 | 1,430 |
| anthropic/claude-haiku-5.5 | devtools | 27 | 0.481 | 0.852 | 14.8% | 1.000 | 23 | 2,967 |
| anthropic/claude-haiku-5.5 | ecommerce | 48 | 0.854 | 0.896 | 10.4% | 0.930 | 43 | 1,909 |
| anthropic/claude-haiku-5.5 | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 815 |
| anthropic/claude-haiku-5.5 | payments | 21 | 0.714 | 0.952 | 4.8% | 1.000 | 20 | 1,980 |
| anthropic/claude-haiku-5.5 | social-media | 30 | 0.467 | 0.900 | 10.0% | 1.000 | 27 | 4,714 |
| anthropic/claude-haiku-5.5 | travel | 18 | 0.944 | 1.000 | 0.0% | 1.000 | 18 | 2,825 |
| openai/gpt-6-luna | devtools | 27 | 0.741 | 0.889 | 11.1% | 1.000 | 24 | 4,023 |
| openai/gpt-6-luna | ecommerce | 48 | 0.812 | 0.917 | 8.3% | 0.932 | 44 | 2,191 |
| openai/gpt-6-luna | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,228 |
| openai/gpt-6-luna | payments | 21 | 0.667 | 1.000 | 0.0% | 1.000 | 21 | 2,595 |
| openai/gpt-6-luna | social-media | 30 | 0.433 | 0.900 | 10.0% | 1.000 | 27 | 3,809 |
| openai/gpt-6-luna | travel | 18 | 0.889 | 1.000 | 0.0% | 1.000 | 18 | 3,467 |
| qwen/qwen3.8-flash | devtools | 27 | 0.333 | 0.704 | 29.6% | 1.000 | 19 | 12,644 |
| qwen/qwen3.8-flash | ecommerce | 48 | 0.604 | 0.896 | 10.4% | 0.907 | 43 | 4,807 |
| qwen/qwen3.8-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,625 |
| qwen/qwen3.8-flash | payments | 21 | 0.476 | 1.000 | 0.0% | 1.000 | 21 | 7,311 |
| qwen/qwen3.8-flash | social-media | 30 | 0.233 | 1.000 | 0.0% | 1.000 | 30 | 10,441 |
| qwen/qwen3.8-flash | travel | 18 | 0.944 | 1.000 | 0.0% | 0.722 | 18 | 9,010 |
| nvidia/nemotron-3.5-super-vl | devtools | 27 | 0.259 | 0.667 | 33.3% | 0.944 | 18 | 6,968 |
| nvidia/nemotron-3.5-super-vl | ecommerce | 48 | 0.625 | 0.875 | 12.5% | 0.929 | 42 | 3,088 |
| nvidia/nemotron-3.5-super-vl | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 792 |
| nvidia/nemotron-3.5-super-vl | payments | 21 | 0.190 | 0.952 | 4.8% | 1.000 | 20 | 4,349 |
| nvidia/nemotron-3.5-super-vl | social-media | 30 | 0.033 | 0.767 | 23.3% | 1.000 | 23 | 4,712 |
| nvidia/nemotron-3.5-super-vl | travel | 18 | 0.944 | 1.000 | 0.0% | 0.944 | 18 | 2,379 |
| z-ai/glm-5.3-flash | devtools | 27 | 0.296 | 0.667 | 33.3% | 1.000 | 18 | 13,954 |
| z-ai/glm-5.3-flash | ecommerce | 48 | 0.667 | 0.854 | 14.6% | 0.902 | 41 | 5,125 |
| z-ai/glm-5.3-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 1,858 |
| z-ai/glm-5.3-flash | payments | 21 | 0.476 | 0.905 | 9.5% | 1.000 | 19 | 5,123 |
| z-ai/glm-5.3-flash | social-media | 30 | 0.133 | 0.800 | 20.0% | 0.917 | 24 | 12,118 |
| z-ai/glm-5.3-flash | travel | 18 | 0.444 | 0.833 | 16.7% | 1.000 | 15 | 13,098 |
| deepseek/deepseek-v4.1-flash-1017 | devtools | 27 | 0.444 | 0.815 | 18.5% | 1.000 | 22 | 3,905 |
| deepseek/deepseek-v4.1-flash-1017 | ecommerce | 48 | 0.750 | 0.938 | 6.2% | 0.933 | 45 | 1,788 |
| deepseek/deepseek-v4.1-flash-1017 | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 719 |
| deepseek/deepseek-v4.1-flash-1017 | payments | 21 | 0.286 | 1.000 | 0.0% | 1.000 | 21 | 2,168 |
| deepseek/deepseek-v4.1-flash-1017 | social-media | 30 | 0.433 | 0.933 | 6.7% | 1.000 | 28 | 3,749 |
| deepseek/deepseek-v4.1-flash-1017 | travel | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 4,203 |
| minimax/minimax-m3.1-flash | devtools | 27 | 0.222 | 0.667 | 33.3% | 1.000 | 18 | 3,206 |
| minimax/minimax-m3.1-flash | ecommerce | 48 | 0.688 | 0.958 | 4.2% | 0.935 | 46 | 1,455 |
| minimax/minimax-m3.1-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 597 |
| minimax/minimax-m3.1-flash | payments | 21 | 0.143 | 0.952 | 4.8% | 1.000 | 20 | 1,362 |
| minimax/minimax-m3.1-flash | social-media | 30 | 0.167 | 0.767 | 23.3% | 1.000 | 23 | 4,219 |
| minimax/minimax-m3.1-flash | travel | 18 | 0.444 | 0.889 | 11.1% | 1.000 | 16 | 2,471 |
| reflection/beam-1.0-501b-a23b | devtools | 27 | 0.333 | 0.370 | 63.0% | 1.000 | 10 | 4,883 |
| reflection/beam-1.0-501b-a23b | ecommerce | 48 | 0.625 | 0.667 | 33.3% | 0.938 | 32 | 4,559 |
| reflection/beam-1.0-501b-a23b | generic | 18 | 0.833 | 0.833 | 16.7% | 1.000 | 15 | 1,037 |
| reflection/beam-1.0-501b-a23b | payments | 21 | 0.381 | 0.524 | 47.6% | 1.000 | 11 | 5,288 |
| reflection/beam-1.0-501b-a23b | social-media | 30 | 0.267 | 0.300 | 70.0% | 1.000 | 9 | 5,515 |
| reflection/beam-1.0-501b-a23b | travel | 18 | 0.333 | 0.333 | 66.7% | 1.000 | 6 | 4,687 |
| google/gemini-3.8-flash | devtools | 27 | 0.519 | 0.556 | 44.4% | 1.000 | 15 | 10,351 |
| google/gemini-3.8-flash | ecommerce | 48 | 0.729 | 0.729 | 27.1% | 0.914 | 35 | 8,390 |
| google/gemini-3.8-flash | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 3,046 |
| google/gemini-3.8-flash | payments | 21 | 0.571 | 0.619 | 38.1% | 1.000 | 13 | 10,437 |
| google/gemini-3.8-flash | social-media | 30 | 0.567 | 0.567 | 43.3% | 1.000 | 17 | 10,910 |
| google/gemini-3.8-flash | travel | 18 | 0.611 | 0.611 | 38.9% | 1.000 | 11 | 11,566 |
| thinkingmachines/inkling-small@nojson | devtools | 27 | 0.259 | 0.630 | 37.0% | 1.000 | 17 | 2,973 |
| thinkingmachines/inkling-small@nojson | ecommerce | 48 | 0.500 | 0.875 | 12.5% | 0.929 | 42 | 1,322 |
| thinkingmachines/inkling-small@nojson | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 506 |
| thinkingmachines/inkling-small@nojson | payments | 21 | 0.095 | 0.952 | 4.8% | 1.000 | 20 | 1,149 |
| thinkingmachines/inkling-small@nojson | social-media | 30 | 0.000 | 0.667 | 33.3% | 1.000 | 20 | 2,817 |
| thinkingmachines/inkling-small@nojson | travel | 18 | 0.889 | 1.000 | 0.0% | 0.833 | 18 | 2,137 |
| thinkingmachines/inkling-small | devtools | 27 | 0.111 | 0.519 | 48.1% | 0.929 | 14 | 7,169 |
| thinkingmachines/inkling-small | ecommerce | 48 | 0.479 | 0.812 | 18.8% | 0.923 | 39 | 1,917 |
| thinkingmachines/inkling-small | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 540 |
| thinkingmachines/inkling-small | payments | 21 | 0.095 | 1.000 | 0.0% | 1.000 | 21 | 1,642 |
| thinkingmachines/inkling-small | social-media | 30 | 0.000 | 0.833 | 16.7% | 1.000 | 25 | 5,154 |
| thinkingmachines/inkling-small | travel | 18 | 0.389 | 1.000 | 0.0% | 0.889 | 18 | 3,219 |
| thinkingmachines/inkling@nojson | devtools | 27 | 0.296 | 0.741 | 25.9% | 1.000 | 20 | 4,416 |
| thinkingmachines/inkling@nojson | ecommerce | 48 | 0.625 | 0.938 | 6.2% | 0.933 | 45 | 1,690 |
| thinkingmachines/inkling@nojson | generic | 18 | 1.000 | 1.000 | 0.0% | 1.000 | 18 | 381 |
| thinkingmachines/inkling@nojson | payments | 21 | 0.000 | 1.000 | 0.0% | 1.000 | 21 | 1,448 |
| thinkingmachines/inkling@nojson | social-media | 30 | 0.167 | 0.867 | 13.3% | 1.000 | 26 | 5,517 |
| thinkingmachines/inkling@nojson | travel | 18 | 0.889 | 1.000 | 0.0% | 0.889 | 18 | 4,352 |
| thinkingmachines/inkling | devtools | 27 | 0.000 | 0.000 | 100.0% | — | 0 | 17,831 |
| thinkingmachines/inkling | ecommerce | 48 | 0.000 | 0.000 | 100.0% | — | 0 | 6,683 |
| thinkingmachines/inkling | generic | 18 | 0.000 | 0.000 | 100.0% | — | 0 | 1,226 |
| thinkingmachines/inkling | payments | 21 | 0.000 | 0.000 | 100.0% | — | 0 | 5,696 |
| thinkingmachines/inkling | social-media | 30 | 0.000 | 0.000 | 100.0% | — | 0 | 19,833 |
| thinkingmachines/inkling | travel | 18 | 0.000 | 0.000 | 100.0% | — | 0 | 15,690 |

## 4. Conformance by keyword category (non-fallback samples)

n = keyword checks in each cell (`checked`); pass rate = passed / checked.

| Model | format | enum | constraint |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 0.974 (n=194) | 1.000 (n=557) | 1.000 (n=1,412) |
| deepseek/deepseek-v4.1-flash | 0.993 (n=451) | 1.000 (n=1,497) | 1.000 (n=3,052) |
| inception/mercury-3 | 0.979 (n=430) | 1.000 (n=1,256) | 1.000 (n=2,769) |
| anthropic/claude-haiku-5.5 | 0.991 (n=334) | 1.000 (n=1,107) | 1.000 (n=2,406) |
| openai/gpt-6-luna | 0.990 (n=313) | 1.000 (n=1,037) | 1.000 (n=2,189) |
| qwen/qwen3.8-flash | 0.973 (n=366) | 1.000 (n=1,173) | 1.000 (n=2,499) |
| nvidia/nemotron-3.5-super-vl | 0.988 (n=420) | 1.000 (n=1,138) | 1.000 (n=2,595) |
| z-ai/glm-5.3-flash | 0.979 (n=381) | 1.000 (n=1,060) | 1.000 (n=2,465) |
| deepseek/deepseek-v4.1-flash-1017 | 0.993 (n=461) | 1.000 (n=1,448) | 1.000 (n=3,074) |
| minimax/minimax-m3.1-flash | 0.992 (n=380) | 1.000 (n=1,065) | 1.000 (n=2,206) |
| reflection/beam-1.0-501b-a23b | 0.982 (n=112) | 1.000 (n=319) | 1.000 (n=641) |
| google/gemini-3.8-flash | 0.986 (n=215) | 1.000 (n=677) | 1.000 (n=1,410) |
| thinkingmachines/inkling-small@nojson | 0.983 (n=348) | 1.000 (n=939) | 1.000 (n=2,035) |
| thinkingmachines/inkling-small | 0.983 (n=416) | 1.000 (n=1,032) | 1.000 (n=2,509) |
| thinkingmachines/inkling@nojson | 0.987 (n=378) | 1.000 (n=1,019) | 1.000 (n=2,422) |
| thinkingmachines/inkling | — (n=0) | — (n=0) | — (n=0) |

## 5. Latency (proxy-measured LLM call times)

n (engine) = successful engine calls; n (all) = successful calls, all workloads; n tok = calls used for the tok/s median. L = e2e p50 over engine calls.

| Model | n (engine) | TTFT p50 | TTFT p95 | TTFR p50 | e2e p50 (engine) | e2e p95 (engine) | n (all) | e2e p50 (all) | e2e p95 (all) | tok/s (engine) | tok/s (all) | n tok |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 227 | 294 | 695 | — | 1,728 | 4,796 | 248 | 1,747 | 4,888 | 269.2 | 267.1 | 242 |
| deepseek/deepseek-v4.1-flash | 172 | 483 | 1,046 | — | 2,204 | 8,229 | 193 | 2,217 | 8,628 | 224.4 | 229.5 | 182 |
| inception/mercury-3 | 177 | 509 | 841 | — | 1,096 | 3,336 | 201 | 1,119 | 3,587 | 772.9 | 772.9 | 167 |
| anthropic/claude-haiku-5.5 | 181 | 531 | 737 | — | 2,502 | 6,473 | 202 | 2,563 | 6,698 | 239.7 | 238.9 | 194 |
| openai/gpt-6-luna | 175 | 906 | 1,496 | — | 2,469 | 7,137 | 196 | 2,620 | 9,181 | 124.9 | 124.9 | 184 |
| qwen/qwen3.8-flash | 163 | 1,057 | 6,301 | — | 7,270 | 25,199 | 184 | 7,952 | 25,648 | 80.3 | 80.4 | 176 |
| nvidia/nemotron-3.5-super-vl | 182 | 835 | 11,874 | — | 2,882 | 14,375 | 204 | 2,882 | 14,679 | 509.7 | 508.1 | 191 |
| z-ai/glm-5.3-flash | 191 | 2,964 | 15,255 | 428 | 7,044 | 27,446 | 213 | 7,067 | 28,866 | 123.6 | 124.5 | 210 |
| deepseek/deepseek-v4.1-flash-1017 | 169 | 535 | 1,733 | — | 2,649 | 9,752 | 190 | 2,656 | 11,541 | 190.2 | 187.4 | 179 |
| minimax/minimax-m3.1-flash | 188 | 531 | 5,736 | 929 | 1,681 | 11,700 | 210 | 1,681 | 11,264 | 278.8 | 299.8 | 178 |
| reflection/beam-1.0-501b-a23b | 162 | 3,854 | 8,625 | 511 | 4,883 | 11,543 | 183 | 4,883 | 12,067 | 416.4 | 422.5 | 176 |
| google/gemini-3.8-flash | 166 | 5,952 | 14,852 | 2,091 | 9,588 | 15,848 | 187 | 9,772 | 16,197 | 211.4 | 211.4 | 187 |
| thinkingmachines/inkling-small@nojson | 193 | 338 | 741 | — | 1,568 | 4,482 | 196 | 1,568 | 5,133 | 269.1 | 269.8 | 187 |
| thinkingmachines/inkling-small | 207 | 340 | 775 | — | 2,458 | 7,510 | 231 | 2,447 | 7,771 | 240.3 | 246.0 | 223 |
| thinkingmachines/inkling@nojson | 184 | 291 | 669 | — | 2,403 | 9,961 | 187 | 2,431 | 10,277 | 107.9 | 108.0 | 175 |
| thinkingmachines/inkling | 312 | 281 | 573 | — | 3,917 | 17,775 | 348 | 4,425 | 19,174 | 99.9 | 101.6 | 348 |

## 6. Reliability

n = ledger rows for the model, budget-refused rows excluded. Provider shares are over calls with a served provider.

| Model | n | errors | error rate | timeouts | timeout rate | engine timeouts (of N) | refusal rate | retry rate | reasoning toggle rejected | JSON mode rejected | response_format sent | served providers (share) | n providers |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 248 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:245 | DeepInfra 49.2%, CoreWeave 33.5%, Io Net 16.9%, Darkbloom 0.4% | 248 |
| deepseek/deepseek-v4.1-flash | 193 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:190 | Wafer 39.9%, CoreWeave 14.0%, Decart 14.0%, Morph 7.3%, Relace 5.7%, DigitalOcean 5.2%, InferenceNet 5.2%, AtlasCloud 3.1%, Parasail 2.1%, Makora 1.6%, Alibaba 0.5%, DeepInfra 0.5%, PrimeIntellect 0.5%, Reka 0.5% | 193 |
| inception/mercury-3 | 201 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:198 | Inception 100.0% | 201 |
| anthropic/claude-haiku-5.5 | 202 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:199 | Claude Platform on AWS 100.0% | 202 |
| openai/gpt-6-luna | 196 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:193 | OpenAI 100.0% | 196 |
| qwen/qwen3.8-flash | 185 | 1 | 0.5% | 0 | 0.0% | 3 (1.9%) | 0.0% | 0.0% | no | no | json_object:182 | Alibaba 100.0% | 185 |
| nvidia/nemotron-3.5-super-vl | 204 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:201 | Darkbloom 24.5%, Crusoe 24.0%, Together 13.7%, DeepInfra 13.2%, Venice 10.3%, CoreWeave 9.3%, Nebius 4.9% | 204 |
| z-ai/glm-5.3-flash | 213 | 0 | 0.0% | 0 | 0.0% | 8 (4.9%) | 0.0% | 1.9% | yes | no | json_object:210 | Wafer 42.7%, OpenInference 41.8%, InferenceNet 5.2%, DigitalOcean 4.2%, Reka 3.8%, Morph 1.4%, Parasail 0.5%, Together 0.5% | 213 |
| deepseek/deepseek-v4.1-flash-1017 | 190 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:187 | Wafer 17.4%, Cohere 14.7%, InferenceNet 13.2%, DigitalOcean 9.5%, Ionstream 8.9%, Modal 4.2%, Baidu 3.7%, Relace 3.7%, Particle AI 3.2%, Alibaba 2.6%, Crusoe 2.6%, ModelRun 2.6%, Morph 2.6%, Parasail 2.6%, StreamLake 2.6%, Cloudflare 1.6%, DeepInfra 1.1%, GMICloud 0.5%, Makora 0.5%, Nebius 0.5%, PrimeIntellect 0.5%, Sail Research 0.5%, Venice 0.5% | 190 |
| minimax/minimax-m3.1-flash | 210 | 0 | 0.0% | 0 | 0.0% | 1 (0.6%) | 0.0% | 1.9% | yes | no | json_object:207 | Parasail 37.1%, DigitalOcean 31.9%, Together 18.6%, Minimax 8.6%, SambaNova 1.9%, GMICloud 1.0%, InferenceNet 1.0% | 210 |
| reflection/beam-1.0-501b-a23b | 184 | 1 | 0.5% | 0 | 0.0% | 0 (0.0%) | 0.0% | 2.2% | yes | no | json_object:181 | Nebius 37.2%, CoreWeave 25.1%, Together 18.0%, DigitalOcean 15.8%, BaseTen 3.8% | 183 |
| google/gemini-3.8-flash | 187 | 0 | 0.0% | 0 | 0.0% | 1 (0.6%) | 0.0% | 2.1% | yes | no | json_object:184 | Google 97.9%, Google AI Studio 2.1% | 187 |
| thinkingmachines/inkling-small@nojson | 196 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:196 | DeepInfra 100.0% | 196 |
| thinkingmachines/inkling-small | 231 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:228 | DeepInfra 100.0% | 231 |
| thinkingmachines/inkling@nojson | 187 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:187 | Together 100.0% | 187 |
| thinkingmachines/inkling | 348 | 0 | 0.0% | 0 | 0.0% | 0 (0.0%) | 0.0% | 0.0% | no | no | json_object:345 | Together 100.0% | 348 |

## 7. Cost

n = engine calls (C, tokens per call); total and actual spend cover all ledger rows of the model.

| Model | engine calls | actual $ (usage) | total $ (all workloads) | engine $ | C $/1k responses | estimate share (engine) | prompt tok/call | completion tok/call | reasoning tok/call | n tok |
|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 227 | 0.0476 | 0.0476 | 0.0425 | 0.2626 | 0.0% | 1,730 | 513 | 0 | 227 |
| deepseek/deepseek-v4.1-flash | 172 | 0.0819 | 0.0819 | 0.0700 | 0.4322 | 0.0% | 1,400 | 515 | 0 | 172 |
| inception/mercury-3 | 177 | 0.0789 | 0.0789 | 0.0537 | 0.3317 | 0.0% | 1,457 | 592 | 0 | 177 |
| anthropic/claude-haiku-5.5 | 181 | 0.1038 | 0.1038 | 0.0907 | 0.5598 | 0.0% | 2,157 | 571 | 0 | 181 |
| openai/gpt-6-luna | 175 | 0.0477 | 0.0477 | 0.0385 | 0.2378 | 0.0% | 1,233 | 277 | 0 | 175 |
| qwen/qwen3.8-flash | 164 | 0.0743 | 0.0743 | 0.0613 | 0.3784 | 0.0% | 1,218 | 602 | 0 | 164 |
| nvidia/nemotron-3.5-super-vl | 182 | 0.1080 | 0.1080 | 0.0873 | 0.5388 | 0.0% | 1,449 | 590 | 0 | 182 |
| z-ai/glm-5.3-flash | 191 | 0.0540 | 0.0540 | 0.0471 | 0.2907 | 0.0% | 1,379 | 964 | 403 | 191 |
| deepseek/deepseek-v4.1-flash-1017 | 169 | 0.0940 | 0.0940 | 0.0801 | 0.4947 | 0.0% | 1,347 | 491 | 0 | 169 |
| minimax/minimax-m3.1-flash | 188 | 0.1009 | 0.1009 | 0.0879 | 0.5426 | 0.0% | 1,430 | 538 | 107 | 188 |
| reflection/beam-1.0-501b-a23b | 163 | 0.3873 | 0.3873 | 0.3181 | 1.9635 | 0.0% | 1,214 | 1,493 | 1,348 | 162 |
| google/gemini-3.8-flash | 166 | 1.1581 | 1.1581 | 1.0025 | 6.1884 | 0.0% | 1,325 | 1,345 | 930 | 166 |
| thinkingmachines/inkling-small@nojson | 193 | 0.1981 | 0.1981 | 0.1903 | 1.1748 | 0.0% | 1,392 | 483 | 0 | 193 |
| thinkingmachines/inkling-small | 207 | 0.2694 | 0.2694 | 0.2138 | 1.3198 | 0.0% | 1,420 | 641 | 0 | 207 |
| thinkingmachines/inkling@nojson | 184 | 0.4951 | 0.4951 | 0.4734 | 2.9225 | 0.0% | 1,370 | 350 | 0 | 184 |
| thinkingmachines/inkling | 312 | 1.2834 | 1.2834 | 1.0464 | 6.4591 | 0.0% | 1,405 | 577 | 0 | 312 |

## 8. Server suites

n = server checks per model (consistency rows, SSE rows, import inputs); import recall/precision are over the import inputs with an expected list.

| Model | consistency n | case ok | field agreement | SSE n | SSE ok | SSE TTFE p50 | import n | draft valid | recall macro / micro | precision macro / micro | import mean ms | warnings mean | harness-rejected rows |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| nvidia/nemotron-3.5-lightning | 18 | 83.3% | 81.5% (fields 65) | 3 | 100.0% | 474 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 9,281 | 0.0 | 0 |
| deepseek/deepseek-v4.1-flash | 18 | 100.0% | 92.6% (fields 81) | 3 | 100.0% | 520 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,621 | 0.0 | 0 |
| inception/mercury-3 | 18 | 83.3% | 74.1% (fields 81) | 3 | 100.0% | 429 | 4 | 50.0% | 0.500 / 0.308 | 0.500 / 1.000 | 1,922 | 0.0 | 0 |
| anthropic/claude-haiku-5.5 | 18 | 100.0% | 92.6% (fields 81) | 3 | 100.0% | 721 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 5,476 | 0.0 | 0 |
| openai/gpt-6-luna | 18 | 88.9% | 82.7% (fields 81) | 3 | 100.0% | 1,183 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 8,349 | 0.0 | 0 |
| qwen/qwen3.8-flash | 18 | 100.0% | 91.4% (fields 81) | 3 | 100.0% | 2,310 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 29,852 | 0.0 | 0 |
| nvidia/nemotron-3.5-super-vl | 18 | 83.3% | 82.7% (fields 81) | 3 | 100.0% | 452 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 10,879 | 0.0 | 0 |
| z-ai/glm-5.3-flash | 18 | 94.4% | 86.4% (fields 81) | 3 | 100.0% | 1,161 | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 28,115 | 0.0 | 0 |
| deepseek/deepseek-v4.1-flash-1017 | 18 | 100.0% | 91.4% (fields 81) | 3 | 100.0% | 1,077 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 7,925 | 0.2 | 0 |
| minimax/minimax-m3.1-flash | 18 | 88.9% | 85.2% (fields 81) | 3 | 100.0% | 805 | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 4,174 | 0.0 | 0 |
| reflection/beam-1.0-501b-a23b | 18 | 100.0% | 88.9% (fields 81) | 3 | 100.0% | 2,966 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 17,026 | 0.0 | 0 |
| google/gemini-3.8-flash | 18 | 100.0% | 92.6% (fields 81) | 3 | 100.0% | 4,066 | 4 | 100.0% | 1.000 / 1.000 | 1.000 / 1.000 | 10,282 | 0.0 | 0 |
| thinkingmachines/inkling-small@nojson | 0 | — | — (fields 0) | 0 | — | — | 4 | 75.0% | 0.750 / 0.615 | 0.750 / 1.000 | 2,731 | 0.0 | 21 |
| thinkingmachines/inkling-small | 18 | 77.8% | 72.6% (fields 73) | 3 | 100.0% | 376 | 4 | 100.0% | 0.800 / 0.692 | 1.000 / 1.000 | 5,665 | 0.2 | 0 |
| thinkingmachines/inkling@nojson | 0 | — | — (fields 0) | 0 | — | — | 4 | 25.0% | 0.250 / 0.077 | 0.250 / 1.000 | 9,213 | 0.0 | 21 |
| thinkingmachines/inkling | 18 | 50.0% | 49.4% (fields 81) | 3 | 100.0% | 511 | 4 | 25.0% | 0.250 / 0.077 | 0.250 / 1.000 | 27,130 | 0.0 | 0 |

harness-rejected rows = server rows Mockingbird refused before any LLM call (the bench's own request was invalid, e.g. `@` from a variant id in the session header) plus the rows skipped because of them; they are left out of every rate above.

Consistency cases by name (n, ok):

| Model | case | n | ok rate |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | square.create | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | square.read | 3 | 33.3% |
| nvidia/nemotron-3.5-lightning | stripe.create | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | stripe.read | 3 | 100.0% |
| nvidia/nemotron-3.5-lightning | tmdb.detail | 3 | 66.7% |
| nvidia/nemotron-3.5-lightning | tmdb.list | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | square.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | square.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | stripe.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | stripe.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | tmdb.detail | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash | tmdb.list | 3 | 100.0% |
| inception/mercury-3 | square.create | 3 | 100.0% |
| inception/mercury-3 | square.read | 3 | 100.0% |
| inception/mercury-3 | stripe.create | 3 | 100.0% |
| inception/mercury-3 | stripe.read | 3 | 100.0% |
| inception/mercury-3 | tmdb.detail | 3 | 0.0% |
| inception/mercury-3 | tmdb.list | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | square.create | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | square.read | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | stripe.create | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | stripe.read | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | tmdb.detail | 3 | 100.0% |
| anthropic/claude-haiku-5.5 | tmdb.list | 3 | 100.0% |
| openai/gpt-6-luna | square.create | 3 | 100.0% |
| openai/gpt-6-luna | square.read | 3 | 100.0% |
| openai/gpt-6-luna | stripe.create | 3 | 100.0% |
| openai/gpt-6-luna | stripe.read | 3 | 100.0% |
| openai/gpt-6-luna | tmdb.detail | 3 | 33.3% |
| openai/gpt-6-luna | tmdb.list | 3 | 100.0% |
| qwen/qwen3.8-flash | square.create | 3 | 100.0% |
| qwen/qwen3.8-flash | square.read | 3 | 100.0% |
| qwen/qwen3.8-flash | stripe.create | 3 | 100.0% |
| qwen/qwen3.8-flash | stripe.read | 3 | 100.0% |
| qwen/qwen3.8-flash | tmdb.detail | 3 | 100.0% |
| qwen/qwen3.8-flash | tmdb.list | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | square.create | 3 | 66.7% |
| nvidia/nemotron-3.5-super-vl | square.read | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | stripe.create | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | stripe.read | 3 | 100.0% |
| nvidia/nemotron-3.5-super-vl | tmdb.detail | 3 | 33.3% |
| nvidia/nemotron-3.5-super-vl | tmdb.list | 3 | 100.0% |
| z-ai/glm-5.3-flash | square.create | 3 | 100.0% |
| z-ai/glm-5.3-flash | square.read | 3 | 100.0% |
| z-ai/glm-5.3-flash | stripe.create | 3 | 100.0% |
| z-ai/glm-5.3-flash | stripe.read | 3 | 100.0% |
| z-ai/glm-5.3-flash | tmdb.detail | 3 | 66.7% |
| z-ai/glm-5.3-flash | tmdb.list | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | square.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | square.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | stripe.create | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | stripe.read | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | tmdb.detail | 3 | 100.0% |
| deepseek/deepseek-v4.1-flash-1017 | tmdb.list | 3 | 100.0% |
| minimax/minimax-m3.1-flash | square.create | 3 | 100.0% |
| minimax/minimax-m3.1-flash | square.read | 3 | 100.0% |
| minimax/minimax-m3.1-flash | stripe.create | 3 | 100.0% |
| minimax/minimax-m3.1-flash | stripe.read | 3 | 100.0% |
| minimax/minimax-m3.1-flash | tmdb.detail | 3 | 33.3% |
| minimax/minimax-m3.1-flash | tmdb.list | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | square.create | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | square.read | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | stripe.create | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | stripe.read | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | tmdb.detail | 3 | 100.0% |
| reflection/beam-1.0-501b-a23b | tmdb.list | 3 | 100.0% |
| google/gemini-3.8-flash | square.create | 3 | 100.0% |
| google/gemini-3.8-flash | square.read | 3 | 100.0% |
| google/gemini-3.8-flash | stripe.create | 3 | 100.0% |
| google/gemini-3.8-flash | stripe.read | 3 | 100.0% |
| google/gemini-3.8-flash | tmdb.detail | 3 | 100.0% |
| google/gemini-3.8-flash | tmdb.list | 3 | 100.0% |
| thinkingmachines/inkling-small | square.create | 3 | 100.0% |
| thinkingmachines/inkling-small | square.read | 3 | 66.7% |
| thinkingmachines/inkling-small | stripe.create | 3 | 100.0% |
| thinkingmachines/inkling-small | stripe.read | 3 | 100.0% |
| thinkingmachines/inkling-small | tmdb.detail | 3 | 0.0% |
| thinkingmachines/inkling-small | tmdb.list | 3 | 100.0% |
| thinkingmachines/inkling | square.create | 3 | 0.0% |
| thinkingmachines/inkling | square.read | 3 | 0.0% |
| thinkingmachines/inkling | stripe.create | 3 | 100.0% |
| thinkingmachines/inkling | stripe.read | 3 | 100.0% |
| thinkingmachines/inkling | tmdb.detail | 3 | 0.0% |
| thinkingmachines/inkling | tmdb.list | 3 | 100.0% |

## 9. Value sensitivity

Value = 100 (Q/Qb)^a / ((C/Cb)^b (L/Lb)^c). n = models with a Value under each set (gated models show —). Default (a,b,c) = (2, 0.5, 0.5).

| Model | (default) | (1,1,1) | (2,0.25,1) |
|---|---|---|---|
| nvidia/nemotron-3.5-lightning | — (rank —) | — (rank —) | — (rank —) |
| deepseek/deepseek-v4.1-flash | 100.0 (rank 2) | 100.0 (rank 2) | 100.0 (rank 2) |
| inception/mercury-3 | — (rank —) | — (rank —) | — (rank —) |
| anthropic/claude-haiku-5.5 | 84.1 (rank 4) | 68.7 (rank 4) | 84.2 (rank 3) |
| openai/gpt-6-luna | 125.1 (rank 1) | 160.7 (rank 1) | 101.8 (rank 1) |
| qwen/qwen3.8-flash | — (rank —) | — (rank —) | — (rank —) |
| nvidia/nemotron-3.5-super-vl | — (rank —) | — (rank —) | — (rank —) |
| z-ai/glm-5.3-flash | — (rank —) | — (rank —) | — (rank —) |
| deepseek/deepseek-v4.1-flash-1017 | 87.4 (rank 3) | 73.6 (rank 3) | 82.4 (rank 4) |
| minimax/minimax-m3.1-flash | — (rank —) | — (rank —) | — (rank —) |
| reflection/beam-1.0-501b-a23b | — (rank —) | — (rank —) | — (rank —) |
| google/gemini-3.8-flash | — (rank —) | — (rank —) | — (rank —) |
| thinkingmachines/inkling-small@nojson | — (rank —) | — (rank —) | — (rank —) |
| thinkingmachines/inkling-small | — (rank —) | — (rank —) | — (rank —) |
| thinkingmachines/inkling@nojson | 31.9 (rank 5) | 12.6 (rank 5) | 49.2 (rank 5) |
| thinkingmachines/inkling | — (rank —) | — (rank —) | — (rank —) |

## 10. Spend

| Scope | n (calls) | USD (ledger cost_usd sum) |
|---|---|---|
| pilot ledger | 88 | 0.1474 |
| full ledger | 3,375 | 4.5826 |
| combined (pilot + full) | 3,463 | 4.7300 |
| superseded ledger (redone runs) | 421 | 0.6578 |
| all incl. superseded | 3,884 | 5.3878 |

By workload (n = ledger rows per scope is in the table above):

| Scope | workload | USD |
|---|---|---|
| pilot | pilot | 0.1474 |
| full | consistency | 0.4724 |
| full | engine | 3.9038 |
| full | import | 0.1894 |
| full | sse | 0.0170 |
| combined | consistency | 0.4724 |
| combined | engine | 3.9038 |
| combined | import | 0.1894 |
| combined | pilot | 0.1474 |
| combined | sse | 0.0170 |
| superseded | consistency | 0.0000 |
| superseded | engine | 0.6188 |
| superseded | import | 0.0390 |
| superseded | sse | 0.0000 |

Key usage delta (key-after.usage - key-before.usage): 5.3536 USD (before 3.1782, after 8.5318; ledger combined minus key delta -0.6236; ledger incl. superseded minus key delta 0.0342).

## 11. Judge plan and checks

n = judge items 110; planned targets 7; judge model `claude-opus-5-5`; seed `mb-bench-2026-10-29`; order-check MAD 0.017 over 30 pairs (noisy: no).

| stratum | contract | target | api_type | domain | rep |
|---|---|---|---|---|---|
| api_type:rest | rest/travel-booking | createBooking | rest | travel | 0 |
| api_type:graphql | graphql/ecommerce-shopify-admin | Query.shop | graphql | ecommerce | 0 |
| api_type:grpc | grpc/otlp-trace-service | TraceService.Export | grpc | devtools | 0 |
| api_type:soap | soap/accounts-rpc | GetAccount | soap | generic | 0 |
| api_type:asyncapi | asyncapi/orders-amqp-2.6 | orderCreated | asyncapi | ecommerce | 0 |
| domain:travel | rest/travel-booking | getBooking | rest | travel | 0 |
| domain:payments | rest/payments-stripe | PostRefunds | rest | payments | 0 |

judge-key check: 112 key items vs 112 planned, missing 0, extra 0, opaque_id mismatches 0.

## Gates

A model's Value is null when gated: skipped, no_data, schema_valid (SV < 0.90), unreliable (error + timeout > 0.10), unjudged (judge_n < 5), missing_inputs. n = engine samples N.

| Model | N | gate reasons |
|---|---|---|
| nvidia/nemotron-3.5-lightning | 162 | schema_valid |
| deepseek/deepseek-v4.1-flash | 162 | none |
| inception/mercury-3 | 162 | schema_valid |
| anthropic/claude-haiku-5.5 | 162 | none |
| openai/gpt-6-luna | 162 | none |
| qwen/qwen3.8-flash | 162 | schema_valid |
| nvidia/nemotron-3.5-super-vl | 162 | schema_valid |
| z-ai/glm-5.3-flash | 162 | schema_valid |
| deepseek/deepseek-v4.1-flash-1017 | 162 | none |
| minimax/minimax-m3.1-flash | 162 | schema_valid |
| reflection/beam-1.0-501b-a23b | 162 | schema_valid |
| google/gemini-3.8-flash | 162 | schema_valid |
| thinkingmachines/inkling-small@nojson | 162 | schema_valid |
| thinkingmachines/inkling-small | 162 | schema_valid |
| thinkingmachines/inkling@nojson | 162 | none |
| thinkingmachines/inkling | 162 | schema_valid |

