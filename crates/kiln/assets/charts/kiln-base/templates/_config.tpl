{{/* The app's config: files (templated) mounted at `config.mountPath`, and env (templated) in a ConfigMap of its own. */}}
{{- define "kiln-base.configMaps" -}}
{{- $top := . -}}
{{- if .Values.config.files }}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "kiln-base.partName" (dict "top" . "part" "config") }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
data:
  {{- range $name, $content := .Values.config.files }}
  {{ $name }}: |
    {{- tpl $content $top | nindent 4 }}
  {{- end }}
{{- end }}
{{- if .Values.config.env }}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "kiln-base.partName" (dict "top" . "part" "env") }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
data:
  {{- range $name, $value := .Values.config.env }}
  {{ $name }}: {{ tpl (toString $value) $top | quote }}
  {{- end }}
{{- end }}
{{- end -}}
