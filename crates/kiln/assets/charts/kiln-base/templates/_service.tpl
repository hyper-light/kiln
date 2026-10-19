{{/* The app's Service: its port to the container's `http`. */}}
{{- define "kiln-base.service" -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
  {{- with .Values.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.service.type }}
  selector:
    {{- include "kiln-base.selectorLabels" . | nindent 4 }}
  ports:
    - name: http
      port: {{ .Values.service.port }}
      targetPort: http
      protocol: TCP
{{- end -}}

{{/* The app's Ingress: each host's paths to the Service. */}}
{{- define "kiln-base.ingress" -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "kiln-base.name" . }}
  labels:
    {{- include "kiln-base.labels" . | nindent 4 }}
  {{- with .Values.ingress.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with .Values.ingress.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with .Values.ingress.tls }}
  tls:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  rules:
    {{- range .Values.ingress.hosts }}
    - host: {{ .host | quote }}
      http:
        paths:
          {{- range (default (list "/") .paths) }}
          - path: {{ . }}
            pathType: Prefix
            backend:
              service:
                name: {{ include "kiln-base.name" $ }}
                port:
                  name: http
          {{- end }}
    {{- end }}
{{- end -}}
