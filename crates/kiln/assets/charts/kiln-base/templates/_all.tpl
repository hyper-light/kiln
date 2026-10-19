{{/* Everything the app's values turn on, as one stream of objects: an app chart's only template. */}}
{{- define "kiln-base.all" -}}
{{- if .Values.serviceAccount.create }}
---
{{ include "kiln-base.serviceAccount" . }}
{{- end }}
{{- include "kiln-base.configMaps" . }}
{{- include "kiln-base.persistence" . }}
---
{{ include "kiln-base.deployment" . }}
{{- if .Values.service.enabled }}
---
{{ include "kiln-base.service" . }}
{{- end }}
{{- if and .Values.ingress.enabled .Values.service.enabled }}
---
{{ include "kiln-base.ingress" . }}
{{- end }}
{{- if .Values.autoscaling.enabled }}
---
{{ include "kiln-base.hpa" . }}
{{- end }}
{{- if .Values.pdb.enabled }}
---
{{ include "kiln-base.pdb" . }}
{{- end }}
{{- if .Values.networkPolicy.enabled }}
---
{{ include "kiln-base.networkPolicy" . }}
{{- end }}
{{- include "kiln-base.cronJobs" . }}
{{- include "kiln-base.jobs" . }}
{{- end -}}
