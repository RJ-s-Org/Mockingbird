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

{{/* The merged config as a dict: files/default-config.yaml with .Values.config over it. */}}
{{- define "mockingbird.configDict" -}}
{{- $defaults := .Files.Get "files/default-config.yaml" | fromYaml -}}
{{- $merged := mergeOverwrite $defaults (deepCopy (default (dict) .Values.config)) -}}
{{- toYaml $merged -}}
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
        checksum/config: {{ include "mockingbird.configDict" $root | sha256sum }}
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
      containers:
        - name: mockingbird
          image: {{ include "mockingbird.image" $root | quote }}
          imagePullPolicy: {{ $root.Values.image.pullPolicy }}
          args: ["serve", "-c", "/etc/mockingbird/mockingbird.yaml", "--role", {{ .role | quote }}]
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
      volumes:
        - name: config
          configMap:
            name: {{ include "mockingbird.fullname" $root }}
        - name: tmp
          emptyDir: {}
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
    {{- toYaml . | nindent 4 }}
  {{- end }}
  rules:
    {{- range $values.hosts }}
    - host: {{ .host | quote }}
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
