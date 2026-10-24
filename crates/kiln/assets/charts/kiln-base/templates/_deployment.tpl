{{/* The app's workload: a Deployment of its image, spread across nodes, restarted when its config changes. */}}
{{- define "kiln-base.deployment" -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
spec:
  {{- if not .Values.autoscaling.enabled }}
  replicas: {{ .Values.replicaCount }}
  {{- end }}
  revisionHistoryLimit: 5
  {{- /* No old pod goes before a new one is ready: a version that can't start costs no capacity,
         and what ran before still serves while the release fails and is put back. */}}
  strategy:
    {{- toYaml (.Values.strategy | default (dict "type" "RollingUpdate" "rollingUpdate" (dict "maxUnavailable" 0 "maxSurge" "25%"))) | nindent 4 }}
  {{- /* No new pod ready for this long: Kubernetes calls the rollout stuck (Degraded), and kiln
         fails the release on it (docs/deploy-api.md §8.1). */}}
  progressDeadlineSeconds: {{ .Values.progressDeadlineSeconds | default 300 }}
  selector:
    matchLabels:
      {{- include "kiln-base.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "kiln-base.labels" . | nindent 8 }}
        {{- with .Values.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      annotations:
        kubectl.kubernetes.io/default-container: app
        {{- /* The versions of the secrets a release pins, where Argo CD deploys it: they roll pods onto new ones. */}}
        {{- with (.Values.kiln | default dict).secrets }}
        kiln.dev/secrets: {{ . | quote }}
        {{- end }}
        {{- if or .Values.config.files .Values.config.env }}
        checksum/config: {{ include "kiln-base.configMaps" . | sha256sum }}
        {{- end }}
        {{- with .Values.podAnnotations }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
    spec:
      {{- include "kiln-base.podCommon" . | nindent 6 }}
      {{- if .Values.topologySpread }}
      topologySpreadConstraints:
        - maxSkew: 1
          topologyKey: kubernetes.io/hostname
          whenUnsatisfiable: ScheduleAnyway
          labelSelector:
            matchLabels:
              {{- include "kiln-base.selectorLabels" . | nindent 14 }}
      {{- end }}
      containers:
        {{- $ports := list (dict "name" "http" "containerPort" (int .Values.port) "protocol" "TCP") }}
        {{- include "kiln-base.container" (dict "top" . "container" (dict "name" "app" "command" .Values.command "args" .Values.args "ports" $ports "probes" .Values.probes "resources" .Values.resources)) | nindent 8 }}
      {{- include "kiln-base.volumes" . | nindent 6 }}
{{- end -}}
