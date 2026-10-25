{{/*
kiln-base: names, labels and the pieces every object shares.
An app chart renders everything its values turn on with one line:
  {{ include "kiln-base.all" . }}
*/}}

{{/* The app's name: the release's (kiln names a release after its app), 63 characters at most. */}}
{{- define "kiln-base.name" -}}
{{- default .Release.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* A part's name: `<app>-<part>`, 63 characters at most. */}}
{{- define "kiln-base.partName" -}}
{{- printf "%s-%s" (include "kiln-base.name" .top) .part | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The labels a pod is selected by. */}}
{{- define "kiln-base.selectorLabels" -}}
app.kubernetes.io/name: {{ include "kiln-base.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* The labels every object carries. */}}
{{- define "kiln-base.labels" -}}
{{ include "kiln-base.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ include "kiln-base.version" . | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- with .Values.labels }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* The version that runs: the image's tag, else the chart's appVersion. */}}
{{- define "kiln-base.version" -}}
{{- default .Chart.AppVersion .Values.image.tag | trunc 63 -}}
{{- end -}}

{{/* The image: by digest when kiln gives one (a release's artifact), else by tag. */}}
{{- define "kiln-base.image" -}}
{{- $i := .Values.image -}}
{{- if $i.digest -}}
{{ $i.repository }}@{{ $i.digest }}
{{- else -}}
{{ $i.repository }}:{{ include "kiln-base.version" . }}
{{- end -}}
{{- end -}}

{{/* The service account pods run as. */}}
{{- define "kiln-base.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "kiln-base.name" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* A container's env: `env` (a map, templated), the config's env, the app's kiln Secret. */}}
{{- define "kiln-base.envFrom" -}}
{{- $out := list -}}
{{- if .Values.config.env -}}
{{- $out = append $out (dict "configMapRef" (dict "name" (include "kiln-base.partName" (dict "top" . "part" "env")))) -}}
{{- end -}}
{{- if .Values.kilnSecrets -}}
{{- /* Named as KilnCD names it, `<release>-kiln`; optional, as an app without secrets has none. */ -}}
{{- $out = append $out (dict "secretRef" (dict "name" (printf "%s-kiln" .Release.Name) "optional" true)) -}}
{{- end -}}
{{- $out = concat $out (default list .Values.envFrom) -}}
{{- if $out }}
envFrom:
{{- toYaml $out | nindent 2 }}
{{- end }}
{{- end -}}

{{- define "kiln-base.env" -}}
{{- $top := . -}}
{{- if .Values.env }}
env:
{{- range $name, $value := .Values.env }}
  - name: {{ $name }}
  {{- if kindIs "map" $value }}
    valueFrom:
      {{- toYaml $value | nindent 6 }}
  {{- else }}
    value: {{ tpl (toString $value) $top | quote }}
  {{- end }}
{{- end }}
{{- end }}
{{- end -}}

{{/* The volumes every pod has: a writable /tmp (the root filesystem is read-only), the config's files, persistence, and the app's own. */}}
{{- define "kiln-base.volumes" -}}
volumes:
  - name: tmp
    emptyDir: {}
{{- if .Values.config.files }}
  - name: config
    configMap:
      name: {{ include "kiln-base.partName" (dict "top" . "part" "config") }}
{{- end }}
{{- range .Values.persistence }}
  - name: {{ .name }}
    persistentVolumeClaim:
      claimName: {{ include "kiln-base.partName" (dict "top" $ "part" .name) }}
{{- end }}
{{- with .Values.volumes }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{- define "kiln-base.volumeMounts" -}}
volumeMounts:
  - name: tmp
    mountPath: /tmp
{{- if .Values.config.files }}
  - name: config
    mountPath: {{ .Values.config.mountPath }}
    readOnly: true
{{- end }}
{{- range .Values.persistence }}
  - name: {{ .name }}
    mountPath: {{ .mountPath }}
{{- end }}
{{- with .Values.volumeMounts }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* The pod-level settings every pod shares. */}}
{{- define "kiln-base.podCommon" -}}
serviceAccountName: {{ include "kiln-base.serviceAccountName" . }}
automountServiceAccountToken: {{ .Values.serviceAccount.automount }}
{{- with .Values.podSecurityContext }}
securityContext:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.affinity }}
affinity:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}

{{/* A container running the app's image with the shared settings; `.container` holds what differs (name, command, args, ports, probes, resources). */}}
{{- define "kiln-base.container" -}}
{{- $top := .top -}}
{{- $c := .container -}}
- name: {{ $c.name }}
  image: {{ include "kiln-base.image" $top }}
  imagePullPolicy: {{ $top.Values.image.pullPolicy }}
  {{- with $c.command }}
  command:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with $c.args }}
  args:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with $c.ports }}
  ports:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with (include "kiln-base.env" $top | trim) }}
  {{- . | nindent 2 }}
  {{- end }}
  {{- with (include "kiln-base.envFrom" $top | trim) }}
  {{- . | nindent 2 }}
  {{- end }}
  {{- with $c.probes }}
  {{- with .liveness }}
  livenessProbe:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .readiness }}
  readinessProbe:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .startup }}
  startupProbe:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- end }}
  {{- with $c.resources }}
  resources:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with $top.Values.securityContext }}
  securityContext:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- include "kiln-base.volumeMounts" $top | nindent 2 }}
{{- end -}}
