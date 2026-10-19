{{/* Cron jobs, by name: `schedule`, `command`, `args`; `resources`, `concurrencyPolicy` (Forbid), `timeZone` when given. */}}
{{- define "kiln-base.cronJobs" -}}
{{- range $name, $j := .Values.cronJobs }}
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: {{ include "kiln-base.partName" (dict "top" $ "part" $name) }}
  labels:
    {{- include "kiln-base.labels" $ | nindent 4 }}
spec:
  schedule: {{ $j.schedule | quote }}
  {{- with $j.timeZone }}
  timeZone: {{ . | quote }}
  {{- end }}
  concurrencyPolicy: {{ default "Forbid" $j.concurrencyPolicy }}
  successfulJobsHistoryLimit: 3
  failedJobsHistoryLimit: 3
  jobTemplate:
    spec:
      backoffLimit: {{ default 1 $j.backoffLimit }}
      template:
        metadata:
          labels:
            {{- include "kiln-base.labels" $ | nindent 12 }}
        spec:
          restartPolicy: Never
          {{- include "kiln-base.podCommon" $ | nindent 10 }}
          containers:
            {{- include "kiln-base.container" (dict "top" $ "container" (dict "name" $name "command" $j.command "args" $j.args "resources" (default $.Values.resources $j.resources))) | nindent 12 }}
          {{- include "kiln-base.volumes" $ | nindent 10 }}
{{- end }}
{{- end -}}

{{/* Jobs a release runs at a moment of its own (Helm's hooks, which Argo CD and KilnCD run too): a migration `hook: pre-upgrade,pre-install`, by name. */}}
{{- define "kiln-base.jobs" -}}
{{- range $name, $j := .Values.jobs }}
---
apiVersion: batch/v1
kind: Job
metadata:
  name: {{ include "kiln-base.partName" (dict "top" $ "part" $name) }}
  labels:
    {{- include "kiln-base.labels" $ | nindent 4 }}
  annotations:
    helm.sh/hook: {{ default "pre-install,pre-upgrade" $j.hook | quote }}
    helm.sh/hook-weight: {{ default 0 $j.weight | quote }}
    helm.sh/hook-delete-policy: before-hook-creation,hook-succeeded
    {{- if $j.reversible }}
    {{- /* What it changes, what ran before also runs on: a release that fails may be reverted. */}}
    kiln.dev/reversible: "true"
    {{- end }}
spec:
  backoffLimit: {{ default 1 $j.backoffLimit }}
  {{- with $j.activeDeadlineSeconds }}
  activeDeadlineSeconds: {{ . }}
  {{- end }}
  template:
    metadata:
      labels:
        {{- include "kiln-base.labels" $ | nindent 8 }}
    spec:
      restartPolicy: Never
      {{- include "kiln-base.podCommon" $ | nindent 6 }}
      containers:
        {{- include "kiln-base.container" (dict "top" $ "container" (dict "name" $name "command" $j.command "args" $j.args "resources" (default $.Values.resources $j.resources))) | nindent 8 }}
      {{- include "kiln-base.volumes" $ | nindent 6 }}
{{- end }}
{{- end -}}
