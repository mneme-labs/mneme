{{/*
Expand the name of the chart.
*/}}
{{- define "mneme.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "mneme.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Common labels applied to all resources.
*/}}
{{- define "mneme.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "mneme.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
Selector labels for Core pods.
*/}}
{{- define "mneme.core.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mneme.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: core
{{- end }}

{{/*
Selector labels for Keeper pods.
*/}}
{{- define "mneme.keeper.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mneme.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: keeper
{{- end }}

{{/*
Selector labels for Replica pods.
*/}}
{{- define "mneme.replica.selectorLabels" -}}
app.kubernetes.io/name: {{ include "mneme.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: replica
{{- end }}

{{/*
Core image reference.
*/}}
{{- define "mneme.core.image" -}}
{{ printf "%s/%s:%s" .Values.image.registry .Values.image.coreRepository .Values.image.tag }}
{{- end }}

{{/*
Keeper image reference.
*/}}
{{- define "mneme.keeper.image" -}}
{{ printf "%s/%s:%s" .Values.image.registry .Values.image.keeperRepository .Values.image.tag }}
{{- end }}

{{/*
Secret name for auth credentials.
*/}}
{{- define "mneme.secretName" -}}
{{- if .Values.auth.existingSecret }}
{{- .Values.auth.existingSecret }}
{{- else }}
{{- printf "%s-auth" (include "mneme.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Namespace for all resources.
*/}}
{{- define "mneme.namespace" -}}
{{- .Values.namespace.name | default .Release.Namespace }}
{{- end }}

{{/*
Core DNS address (used by Keepers + Replicas to connect).
*/}}
{{- define "mneme.core.addr" -}}
{{- printf "%s-core.%s.svc.cluster.local:7379" (include "mneme.fullname" .) (include "mneme.namespace" .) }}
{{- end }}
