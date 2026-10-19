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
  {{- with .Values.strategy }}
  strategy:
    {{- toYaml . | nindent 4 }}
  {{- end }}
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
