{{- define "sc-service.labels" -}}
app.kubernetes.io/name: {{ .Release.Name }}
app.kubernetes.io/part-of: scbackend
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "sc-service.selector" -}}
app.kubernetes.io/name: {{ .Release.Name }}
{{- end }}
