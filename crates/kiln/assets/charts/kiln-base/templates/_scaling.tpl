{{/* Autoscaling on CPU (and memory, when given). */}}
{{- define "kiln-base.hpa" -}}
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: {{ include "kiln-base.name" . }}
  minReplicas: {{ .Values.autoscaling.minReplicas }}
  maxReplicas: {{ .Values.autoscaling.maxReplicas }}
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: {{ .Values.autoscaling.cpu }}
    {{- with .Values.autoscaling.memory }}
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: {{ . }}
    {{- end }}
{{- end -}}

{{/* How many pods a voluntary disruption (a node drained) may take at once. */}}
{{- define "kiln-base.pdb" -}}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
spec:
  {{- if .Values.pdb.maxUnavailable }}
  maxUnavailable: {{ .Values.pdb.maxUnavailable }}
  {{- else }}
  minAvailable: {{ .Values.pdb.minAvailable }}
  {{- end }}
  selector:
    matchLabels:
      {{- include "kiln-base.selectorLabels" . | nindent 6 }}
{{- end -}}

{{- define "kiln-base.serviceAccount" -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "kiln-base.serviceAccountName" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
  {{- with .Values.serviceAccount.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
{{- end -}}
