{{/* Who may reach the app's pods: its own namespace, and the peers `networkPolicy.ingressFrom` names. */}}
{{- define "kiln-base.networkPolicy" -}}
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
spec:
  podSelector:
    matchLabels:
      {{- include "kiln-base.selectorLabels" . | nindent 6 }}
  policyTypes: [Ingress]
  ingress:
    - from:
        - podSelector: {}
        {{- with .Values.networkPolicy.ingressFrom }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      ports:
        - port: http
{{- end -}}

{{/* A claim for each of `persistence`: `name`, `size`, `mountPath`, `storageClass`, `accessMode`. */}}
{{- define "kiln-base.persistence" -}}
{{- range .Values.persistence }}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ include "kiln-base.partName" (dict "top" $ "part" .name) }}
  labels:
    {{- include "kiln-base.labels" $ | nindent 4 }}
spec:
  accessModes: [{{ default "ReadWriteOnce" .accessMode }}]
  {{- with .storageClass }}
  storageClassName: {{ . }}
  {{- end }}
  resources:
    requests:
      storage: {{ .size }}
{{- end }}
{{- end -}}
