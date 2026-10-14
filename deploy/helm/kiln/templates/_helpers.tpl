{{/* vim: set filetype=mustache: */}}

{{- define "kiln.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "kiln.fullname" -}}
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

{{- define "kiln.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "kiln.selectorLabels" -}}
app.kubernetes.io/name: {{ include "kiln.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "kiln.labels" -}}
helm.sh/chart: {{ include "kiln.chart" . }}
{{ include "kiln.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: kiln
{{- end -}}

{{- define "kiln.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "kiln.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "kiln.jobsNamespace" -}}
{{- required "namespaces.jobs is required" .Values.namespaces.jobs -}}
{{- end -}}

{{- define "kiln.adminSecretName" -}}
{{- default (printf "%s-admin" (include "kiln.fullname" .)) .Values.adminToken.existingSecret -}}
{{- end -}}

{{- define "kiln.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- end -}}

{{- define "kiln.serviceHost" -}}
{{- printf "%s.%s.svc" (include "kiln.fullname" .) .Release.Namespace -}}
{{- end -}}

{{/*
Effective kiln.toml as YAML: derived defaults overlaid with .Values.config.
*/}}
{{- define "kiln.effectiveConfig" -}}
{{- $host := include "kiln.serviceHost" . -}}
{{- $inUrl := printf "http://%s:%v" $host .Values.ports.http -}}
{{- $inQuic := printf "%s:%v" $host .Values.ports.quic -}}
{{- $fullname := include "kiln.fullname" . -}}
{{- $tlsNames := list $fullname (printf "%s.%s" $fullname .Release.Namespace) $host (printf "%s.cluster.local" $host) -}}
{{- range .Values.tlsNames }}{{ $tlsNames = append $tlsNames . }}{{ end -}}
{{- $server := dict
  "http_addr" (printf "0.0.0.0:%v" .Values.ports.http)
  "quic_addr" (printf "0.0.0.0:%v" .Values.ports.quic)
  "public_url" (default $inUrl .Values.publicURL)
  "quic_advertise" (default $inQuic .Values.quicAdvertise)
  "tls_names" (uniq $tlsNames)
  "data_dir" .Values.persistence.mountPath -}}
{{- $k8s := dict
  "namespace" (include "kiln.jobsNamespace" .)
  "agent_image" (default (include "kiln.image" .) .Values.jobAgentImage)
  "image_pull_policy" .Values.image.pullPolicy
  "service_account" .Values.jobServiceAccount.name
  "server_url" $inUrl
  "server_quic" $inQuic -}}
{{- if .Values.cache.enabled -}}
{{- $_ := set $k8s "cache_pvc" .Values.cache.name -}}
{{- end -}}
{{- if .Values.priorityClasses.create -}}
{{- $_ := set $k8s "priority_class" .Values.priorityClasses.job -}}
{{- end -}}
{{- $gitops := dict -}}
{{- if .Values.github.existingSecret -}}
{{- $_ := set $gitops "github_token_file" (printf "/etc/kiln/secrets/%s" .Values.github.tokenKey) -}}
{{- if .Values.github.webhookSecretKey -}}
{{- $_ := set $gitops "webhook_secret_file" (printf "/etc/kiln/secrets/%s" .Values.github.webhookSecretKey) -}}
{{- end -}}
{{- end -}}
{{- $derived := dict "server" $server "kubernetes" $k8s "gitops" $gitops -}}
{{- $merged := mergeOverwrite $derived (deepCopy (default (dict) .Values.config)) -}}
{{- toYaml $merged -}}
{{- end -}}

{{/* TOML key: bare when possible, else a quoted (JSON-compatible basic) string. */}}
{{- define "kiln.tomlKey" -}}
{{- if regexMatch "^[A-Za-z0-9_-]+$" . -}}{{ . }}{{- else -}}{{ toJson . }}{{- end -}}
{{- end -}}

{{/* TOML scalar/array value. JSON strings, numbers, booleans and flat arrays are valid TOML. */}}
{{- define "kiln.tomlValue" -}}
{{- toJson . -}}
{{- end -}}

{{/* Render a map of sections ({section: {key: value | {sub: {...}}} | [ {..}, .. ]}) as TOML. */}}
{{- define "kiln.renderToml" -}}
{{- range $section, $body := . }}
{{- if kindIs "slice" $body }}
{{- range $body }}
[[{{ $section }}]]
{{- range $k, $v := . }}
{{- if not (kindIs "invalid" $v) }}
{{ include "kiln.tomlKey" $k }} = {{ include "kiln.tomlValue" $v }}
{{- end }}
{{- end }}
{{ end }}
{{- else if kindIs "map" $body }}
[{{ $section }}]
{{- range $k, $v := $body }}
{{- if and (not (kindIs "map" $v)) (not (kindIs "invalid" $v)) }}
{{ include "kiln.tomlKey" $k }} = {{ include "kiln.tomlValue" $v }}
{{- end }}
{{- end }}
{{- range $k, $v := $body }}
{{- if kindIs "map" $v }}

[{{ include "kiln.tomlKey" $section }}.{{ include "kiln.tomlKey" $k }}]
{{- range $k2, $v2 := $v }}
{{- if not (kindIs "invalid" $v2) }}
{{ include "kiln.tomlKey" $k2 }} = {{ include "kiln.tomlValue" $v2 }}
{{- end }}
{{- end }}
{{- end }}
{{- end }}
{{ end }}
{{- end }}
{{- end -}}

{{- define "kiln.hasCRD" -}}
{{- $ctx := index . 0 -}}
{{- $skip := index . 1 -}}
{{- if or $skip ($ctx.Capabilities.APIVersions.Has "monitoring.coreos.com/v1") -}}true{{- end -}}
{{- end -}}
