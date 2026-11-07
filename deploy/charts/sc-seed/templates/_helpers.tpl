{{- define "sc-seed.labels" -}}
app.kubernetes.io/name: sc-seed
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/part-of: scbackend
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/* The sc-seeder image reference. */}}
{{- define "sc-seed.image" -}}
{{- if kindIs "string" .Values.image -}}
{{ .Values.image }}
{{- else -}}
{{ .Values.image.registry }}/{{ .Values.image.name }}:{{ .Values.image.tag }}
{{- end -}}
{{- end }}

{{/* Whether the Job lists ShardRegions through the API. */}}
{{- define "sc-seed.discovers" -}}
{{- if not .Values.entitygraph }}true{{ end -}}
{{- end }}

{{/* Names a cluster-wide object of this release. */}}
{{- define "sc-seed.clusterName" -}}
{{ printf "%s-%s-shardregions" .Release.Namespace .Release.Name | trunc 63 | trimSuffix "-" }}
{{- end }}
