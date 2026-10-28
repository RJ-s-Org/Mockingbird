{{/* The chart name, overridable with nameOverride. */}}
{{- define "mockingbird.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The release's full name, the base of every object name. */}}
{{- define "mockingbird.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "mockingbird.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Labels shared by every object. */}}
{{- define "mockingbird.labels" -}}
helm.sh/chart: {{ include "mockingbird.chart" . }}
{{ include "mockingbird.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "mockingbird.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mockingbird.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Selector labels of one component (all, control or data); takes (dict "root" $ "component" "data"). */}}
{{- define "mockingbird.componentSelectorLabels" -}}
{{ include "mockingbird.selectorLabels" .root }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/* The component serving the mocks: `all` or `data`. */}}
{{- define "mockingbird.dataComponent" -}}
{{- if eq .Values.mode "split" -}}data{{- else -}}all{{- end -}}
{{- end -}}

{{/* The component serving the console and the Admin API: `all` or `control`. */}}
{{- define "mockingbird.adminComponent" -}}
{{- if eq .Values.mode "split" -}}control{{- else -}}all{{- end -}}
{{- end -}}

{{- define "mockingbird.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "mockingbird.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* The image reference: the tag defaults to appVersion, and `slim` appends -slim. */}}
{{- define "mockingbird.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if eq .Values.image.flavor "slim" -}}
{{- $tag = printf "%s-slim" $tag -}}
{{- end -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/*
The merged config as a dict: files/default-config.yaml, then .Values.config,
then the mocks and LLM profiles declared in .Values.mocks and .Values.llm.profiles.
*/}}
{{- define "mockingbird.configDict" -}}
{{- $defaults := .Files.Get "files/default-config.yaml" | fromYaml -}}
{{- $merged := mergeOverwrite $defaults (deepCopy (default (dict) .Values.config)) -}}
{{- $mocks := default (dict) (get $merged "mocks") -}}
{{- range $name, $mock := .Values.mocks }}
{{- $entry := omit (deepCopy $mock) "contract" -}}
{{- $_ := set $entry "contract" (include "mockingbird.contractPath" (dict "name" $name "contract" $mock.contract)) -}}
{{- $_ := set $mocks $name (mergeOverwrite (default (dict) (get $mocks $name)) $entry) -}}
{{- end }}
{{- if $mocks }}
{{- $_ := set $merged "mocks" $mocks -}}
{{- end }}
{{- if .Values.llm.profiles }}
{{- $llm := default (dict) (get $merged "llm") -}}
{{- $profiles := default (dict) (get $llm "profiles") -}}
{{- range $name, $profile := .Values.llm.profiles }}
{{- $entry := omit (deepCopy $profile) "apiKeySecret" -}}
{{- if $profile.apiKeySecret }}
{{- $_ := set $entry "api_key" (printf "env:%s" (include "mockingbird.profileKeyEnv" $name)) -}}
{{- end }}
{{- $_ := set $profiles $name (mergeOverwrite (default (dict) (get $profiles $name)) $entry) -}}
{{- end }}
{{- $_ := set $llm "profiles" $profiles -}}
{{- $_ := set $merged "llm" $llm -}}
{{- end }}
{{- toYaml $merged -}}
{{- end -}}

{{/* The environment variable carrying an LLM profile's key: MB_PROFILE_KEY_<NAME>. */}}
{{- define "mockingbird.profileKeyEnv" -}}
{{- printf "MB_PROFILE_KEY_%s" (regexReplaceAll "[^A-Z0-9]" (upper .) "_") -}}
{{- end -}}

{{/* Where an inline contract lives in the ConfigMap: key `<mock>.<fileName>`. */}}
{{- define "mockingbird.inlineContractKey" -}}
{{- printf "%s.%s" .name (default "contract.yaml" .contract.fileName) -}}
{{- end -}}

{{/*
The path a mock's contract is served from. Takes (dict "name" <mock> "contract" <contract>).
A string stays as given; `inline` is a key of the main ConfigMap (next to
mockingbird.yaml), `configMap` a volume under /var/lib/mockingbird/contracts and
`url` a file the fetch-contracts init container downloads into /var/lib/mockingbird/fetched.
*/}}
{{- define "mockingbird.contractPath" -}}
{{- $c := .contract -}}
{{- if kindIs "string" $c -}}
{{- $c -}}
{{- else if not (kindIs "map" $c) -}}
{{- fail (printf "mocks.%s.contract is required: a path, or one of inline, configMap or url" .name) -}}
{{- else if hasKey $c "inline" -}}
{{- include "mockingbird.inlineContractKey" . -}}
{{- else if $c.configMap -}}
{{- printf "/var/lib/mockingbird/contracts/%s/%s" .name $c.configMap.key -}}
{{- else if $c.url -}}
{{- printf "/var/lib/mockingbird/fetched/%s/%s" .name (default "contract" $c.fileName) -}}
{{- else -}}
{{- fail (printf "mocks.%s.contract needs one of inline, configMap or url" .name) -}}
{{- end -}}
{{- end -}}

{{/* The main ConfigMap's data: mockingbird.yaml and the inline contracts. */}}
{{- define "mockingbird.configMapData" -}}
mockingbird.yaml: |
  {{- include "mockingbird.configDict" . | nindent 2 }}
{{- range $name, $mock := .Values.mocks }}
{{- if and (kindIs "map" $mock.contract) (hasKey $mock.contract "inline") }}
{{ include "mockingbird.inlineContractKey" (dict "name" $name "contract" $mock.contract) }}: |
  {{- $mock.contract.inline | nindent 2 }}
{{- end }}
{{- end }}
{{- end -}}

{{/* Whether any mock's contract is downloaded from a URL. */}}
{{- define "mockingbird.hasUrlContracts" -}}
{{- range $name, $mock := .Values.mocks }}
{{- if and (kindIs "map" $mock.contract) $mock.contract.url }}true{{ end }}
{{- end }}
{{- end -}}

{{/* The curl image used by the fetch-contracts init container and helm test. */}}
{{- define "mockingbird.curlImage" -}}
{{- printf "%s:%s" .Values.curlImage.repository .Values.curlImage.tag -}}
{{- end -}}

{{/* A port of the merged config's `server` section, or its default. Takes (dict "root" $ "key" "port" "default" 8080). */}}
{{- define "mockingbird.serverPort" -}}
{{- $config := include "mockingbird.configDict" .root | fromYaml -}}
{{- $server := default (dict) (get $config "server") -}}
{{- default .default (get $server .key) -}}
{{- end -}}

{{/* One env var from a {url, existingSecret, existingSecretKey} block. Takes (dict "name" "MB_STORAGE_URL" "value" .Values.storage). */}}
{{- define "mockingbird.urlEnv" -}}
{{- if .value.existingSecret }}
- name: {{ .name }}
  valueFrom:
    secretKeyRef:
      name: {{ .value.existingSecret }}
      key: {{ default "url" .value.existingSecretKey }}
{{- else if .value.url }}
- name: {{ .name }}
  value: {{ .value.url | quote }}
{{- end }}
{{- end -}}

{{/* The container env: stores, bus, LLM key, then .Values.env. */}}
{{- define "mockingbird.env" -}}
{{- include "mockingbird.urlEnv" (dict "name" "MB_STORAGE_URL" "value" .Values.storage) }}
{{- include "mockingbird.urlEnv" (dict "name" "MB_STATE_URL" "value" .Values.state) }}
{{- include "mockingbird.urlEnv" (dict "name" "MB_BUS_URL" "value" .Values.bus) }}
{{- if .Values.llm.existingSecret }}
- name: MB_LLM_API_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.llm.existingSecret }}
      key: {{ .Values.llm.apiKeyKey }}
{{- end }}
{{- with .Values.llm.baseUrl }}
- name: MB_LLM_BASE_URL
  value: {{ . | quote }}
{{- end }}
{{- with .Values.llm.model }}
- name: MB_LLM_MODEL
  value: {{ . | quote }}
{{- end }}
{{- range $name, $profile := .Values.llm.profiles }}
{{- with $profile.apiKeySecret }}
- name: {{ include "mockingbird.profileKeyEnv" $name }}
  valueFrom:
    secretKeyRef:
      name: {{ .name }}
      key: {{ default "api-key" .key }}
{{- end }}
{{- end }}
{{- with .Values.env }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* Fails the render when split mode has no storage. */}}
{{- define "mockingbird.validate" -}}
{{- if eq .Values.mode "split" -}}
{{- $_ := required "mode: split needs storage.url or storage.existingSecret (a postgres:// URL)" (or .Values.storage.url .Values.storage.existingSecret) -}}
{{- else if ne .Values.mode "allInOne" -}}
{{- fail (printf "mode must be allInOne or split, not %q" .Values.mode) -}}
{{- end -}}
{{- if not (has .Values.reload (list "rollout" "watch")) -}}
{{- fail (printf "reload must be rollout or watch, not %q" .Values.reload) -}}
{{- end -}}
{{- end -}}

{{/*
A Deployment for one role. Takes (dict "root" $ "role" "data" "component" "data" "spec" .Values.data "replicas" true).
*/}}
{{- define "mockingbird.deployment" -}}
{{- $root := .root -}}
{{- $spec := .spec -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "mockingbird.fullname" $root }}-{{ .component }}
  labels:
    {{- include "mockingbird.labels" $root | nindent 4 }}
    app.kubernetes.io/component: {{ .component }}
spec:
  {{- if .replicas }}
  replicas: {{ $spec.replicas }}
  {{- end }}
  selector:
    matchLabels:
      {{- include "mockingbird.componentSelectorLabels" (dict "root" $root "component" .component) | nindent 6 }}
  template:
    metadata:
      annotations:
        {{- if eq $root.Values.reload "rollout" }}
        checksum/config: {{ include "mockingbird.configMapData" $root | sha256sum }}
        {{- end }}
        {{- with $spec.podAnnotations }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      labels:
        {{- include "mockingbird.labels" $root | nindent 8 }}
        app.kubernetes.io/component: {{ .component }}
    spec:
      serviceAccountName: {{ include "mockingbird.serviceAccountName" $root }}
      automountServiceAccountToken: false
      {{- with $root.Values.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      terminationGracePeriodSeconds: 30
      {{- if include "mockingbird.hasUrlContracts" $root }}
      initContainers:
        - name: fetch-contracts
          image: {{ include "mockingbird.curlImage" $root | quote }}
          imagePullPolicy: {{ $root.Values.curlImage.pullPolicy }}
          command: ["sh", "-ec"]
          args:
            - |
              {{- range $name, $mock := $root.Values.mocks }}
              {{- if and (kindIs "map" $mock.contract) $mock.contract.url }}
              {{- $path := include "mockingbird.contractPath" (dict "name" $name "contract" $mock.contract) }}
              mkdir -p {{ dir $path | squote }}
              curl -fsSL --retry 3 --max-time 60 -o {{ $path | squote }} {{ $mock.contract.url | squote }}
              {{- with $mock.contract.sha256 }}
              echo {{ printf "%s  %s" . $path | squote }} | sha256sum -c -
              {{- end }}
              {{- end }}
              {{- end }}
          {{- with $root.Values.securityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          volumeMounts:
            - name: fetched-contracts
              mountPath: /var/lib/mockingbird/fetched
      {{- end }}
      containers:
        - name: mockingbird
          image: {{ include "mockingbird.image" $root | quote }}
          imagePullPolicy: {{ $root.Values.image.pullPolicy }}
          args: ["serve", "-c", "/etc/mockingbird/mockingbird.yaml", "--role", {{ .role | quote }}{{ if and (eq $root.Values.reload "watch") (ne .role "data") }}, "--watch"{{ end }}]
          {{- $env := include "mockingbird.env" $root | trim }}
          {{- if $env }}
          env:
            {{- $env | nindent 12 }}
          {{- end }}
          {{- with $root.Values.envFrom }}
          envFrom:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          ports:
            - name: http
              containerPort: {{ include "mockingbird.serverPort" (dict "root" $root "key" "port" "default" 8080) }}
              protocol: TCP
            - name: grpc
              containerPort: {{ include "mockingbird.serverPort" (dict "root" $root "key" "grpc_port" "default" 50051) }}
              protocol: TCP
            - name: admin
              containerPort: {{ include "mockingbird.serverPort" (dict "root" $root "key" "admin_port" "default" 9090) }}
              protocol: TCP
          livenessProbe:
            httpGet:
              path: /healthz
              port: admin
            periodSeconds: 10
            failureThreshold: 3
          readinessProbe:
            httpGet:
              path: /readyz
              port: admin
            periodSeconds: 5
            failureThreshold: 3
          {{- /* The image is distroless (no shell, no sleep binary): the kubelet's own sleep action, from Kubernetes 1.30. */}}
          {{- if semverCompare ">=1.30.0-0" $root.Capabilities.KubeVersion.Version }}
          lifecycle:
            preStop:
              sleep:
                seconds: 5
          {{- end }}
          {{- with $root.Values.securityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          {{- with $spec.resources }}
          resources:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          volumeMounts:
            - name: config
              mountPath: /etc/mockingbird
              readOnly: true
            - name: tmp
              mountPath: /tmp
            {{- range $i, $name := keys $root.Values.mocks | sortAlpha }}
            {{- $contract := (get $root.Values.mocks $name).contract }}
            {{- if and (kindIs "map" $contract) $contract.configMap }}
            - name: contract-{{ $i }}
              mountPath: /var/lib/mockingbird/contracts/{{ $name }}
              readOnly: true
            {{- end }}
            {{- end }}
            {{- if include "mockingbird.hasUrlContracts" $root }}
            - name: fetched-contracts
              mountPath: /var/lib/mockingbird/fetched
              readOnly: true
            {{- end }}
      volumes:
        - name: config
          configMap:
            name: {{ include "mockingbird.fullname" $root }}
        - name: tmp
          emptyDir: {}
        {{- range $i, $name := keys $root.Values.mocks | sortAlpha }}
        {{- $contract := (get $root.Values.mocks $name).contract }}
        {{- if and (kindIs "map" $contract) $contract.configMap }}
        {{- /* The whole ConfigMap, not a subPath, so updates reach the pod (and --watch). */}}
        - name: contract-{{ $i }}
          configMap:
            name: {{ tpl $contract.configMap.name $root }}
            items:
              - key: {{ $contract.configMap.key }}
                path: {{ $contract.configMap.key }}
        {{- end }}
        {{- end }}
        {{- if include "mockingbird.hasUrlContracts" $root }}
        - name: fetched-contracts
          emptyDir: {}
        {{- end }}
      {{- with $spec.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $spec.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $spec.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $spec.topologySpreadConstraints }}
      topologySpreadConstraints:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end -}}

{{/*
An Ingress. Takes (dict "root" $ "values" .Values.ingress "suffix" "" "service" "<name>" "port" "http").
*/}}
{{- define "mockingbird.ingress" -}}
{{- $root := .root -}}
{{- $values := .values -}}
{{- $service := .service -}}
{{- $port := .port -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "mockingbird.fullname" $root }}{{ .suffix }}
  labels:
    {{- include "mockingbird.labels" $root | nindent 4 }}
  {{- with $values.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with $values.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with $values.tls }}
  tls:
    {{- tpl (toYaml .) $root | nindent 4 }}
  {{- end }}
  rules:
    {{- range $values.hosts }}
    - host: {{ tpl .host $root | quote }}
      http:
        paths:
          {{- range .paths }}
          - path: {{ .path }}
            pathType: {{ default "Prefix" .pathType }}
            backend:
              service:
                name: {{ $service }}
                port:
                  name: {{ $port }}
          {{- end }}
    {{- end }}
{{- end -}}

{{/* Whether a PodDisruptionBudget fits a component: more than one replica, or autoscaled. Takes (dict "spec" .Values.data "autoscaled" true). */}}
{{- define "mockingbird.pdbFits" -}}
{{- if or .autoscaled (gt (int .spec.replicas) 1) -}}true{{- end -}}
{{- end -}}
