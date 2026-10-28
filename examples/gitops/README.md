# GitOps examples

Manifests for running Mockingbird from Git; [`docs/deploy/gitops.md`](../../docs/deploy/gitops.md) walks through them.

| File | What |
| --- | --- |
| [`mocks-values.yaml`](mocks-values.yaml) | Chart values with the mocks declared in them, as an app repository keeps them (`deploy/preview/mocks-values.yaml`) |
| [`argocd/application.yaml`](argocd/application.yaml) | Argo CD `Application`s: the chart with values from Git, and the Kustomize staging overlay |
| [`argocd/applicationset-pr.yaml`](argocd/applicationset-pr.yaml) | Argo CD `ApplicationSet` with the pull-request generator: a mock environment per PR |
| [`flux/mocks.yaml`](flux/mocks.yaml) | Flux `GitRepository`, `HelmRelease` (with `helm test`) and a cosign-verified `OCIRepository` |
| [`argo-workflows/workflowtemplate.yaml`](argo-workflows/workflowtemplate.yaml) | Start Mockingbird, run a test container, collect the request log, tear down |
| [`argo-workflows/mocks-configmap.yaml`](argo-workflows/mocks-configmap.yaml) | The config and contract that workflow (and the Tekton task) serve |
| [`tekton/task.yaml`](tekton/task.yaml) | The same as a Tekton `Task` with a Mockingbird sidecar |

Replace `example/shop` with your application's repository and `preview.example.com` with your preview domain.
