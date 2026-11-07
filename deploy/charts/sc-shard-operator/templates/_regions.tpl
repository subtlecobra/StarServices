{{- /* The ShardRegions of the values as the operator's install command reads them. A region that lists no
       gameBuilds gets the chart's build. */}}
{{- define "sc-shard-operator.regions" -}}
{{- $regions := list }}
{{- range .Values.shardRegions }}
{{- $spec := deepCopy (.spec | default dict) }}
{{- if not $spec.gameBuilds }}
{{- $_ := set $spec "gameBuilds" (list ($.Values.build | toString)) }}
{{- end }}
{{- $regions = append $regions (dict "name" .name "spec" $spec) }}
{{- end }}
{{- toYaml $regions }}
{{- end }}

{{- /* The ClusterRoles each region's kube-arangodb is bound to, with -cluster and -member. */}}
{{- define "sc-shard-operator.arangoRole" -}}
{{ .Release.Namespace }}-{{ .Release.Name }}-region-arango
{{- end }}
